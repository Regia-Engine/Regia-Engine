// SPDX-License-Identifier: MIT-0
//! A minimal glTF 2.0 reader for the scene's scatter meshes.
//!
//! The engine's runtime crates have no glTF reader (a game loads cooked meshes); the showcase reads the
//! orchestrator's source meshes directly, as `samples/lookdev` does for its scans. Supported:
//!
//! - `.gltf` (JSON with external `.bin` buffers named by a relative `uri`) and `.glb` (binary container, the
//!   first buffer without a `uri` is the `BIN` chunk); base64 `data:` URIs are read;
//! - float `POSITION`, `NORMAL` (absent: generated from the faces) and `TEXCOORD_0` (absent: zero),
//!   `u8`/`u16`/`u32` indices (absent: a triangle list in vertex order), triangle-list primitives only;
//! - the node hierarchy of the default scene with TRS or matrix transforms (the orchestrator's 1 m
//!   normalisation sits on a root node), baked into the vertices;
//! - levels of detail by node name: a node named `<anything>_LOD<n>` belongs to level `n`, any other node to
//!   level 0; nodes or meshes whose name contains `billboard` are skipped (impostor cards) unless the node names
//!   its level (a tree's `_LOD3` billboard is part of its chain); a LOD node's `extras.lod_screen_size` and a
//!   node's `extras.shadow_lod` (Tincal vegetation) are read into [`Model::lod_screen_sizes`] and
//!   [`Model::shadow_lod`];
//! - metallic-roughness materials: base colour, normal, occlusion and metallic-roughness textures (one packed
//!   ORM image or two images, merged), emissive, `alphaMode` (`MASK` and `BLEND` both draw alpha-tested) and
//!   `doubleSided`. PNG and JPEG images, decoded in parallel and scaled down to a texture cap.
//!
//! Nothing here panics on malformed input: every read is bounds-checked and returns a [`GltfError`].

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use regia_core::math::{Mat3, Mat4, Quat, Vec3};
use regia_render::{ALPHA_MASK, CpuMaterial, LabTexture, LabVertex};
use serde_json::Value;

/// Why a model could not be read: the file and what is wrong with it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct GltfError(pub String);

/// One level of detail of a [`ModelPart`]: a triangle list in object space (metres, +Y up).
#[derive(Clone, Debug)]
pub struct PartLod {
    /// Vertices with the node transforms applied.
    pub vertices: Vec<LabVertex>,
    /// Triangle list into [`Self::vertices`].
    pub indices: Vec<u32>,
    /// The glTF level this came from (the `_LOD<n>` suffix; levels a part lacks are skipped, so the index of a
    /// level in [`ModelPart::lods`] can be lower than this).
    pub source_lod: u32,
}

/// One drawable part of a model: a material and its levels of detail, finest first.
#[derive(Clone, Debug)]
pub struct ModelPart {
    /// The part's material.
    pub material: CpuMaterial,
    /// Levels of detail, finest first, at least one.
    pub lods: Vec<PartLod>,
}

/// A model read from one `.gltf` or `.glb` file.
#[derive(Clone, Debug)]
pub struct Model {
    /// One part per material.
    pub parts: Vec<ModelPart>,
    /// Smallest corner of the finest level's bounds, object space.
    pub bounds_min: [f32; 3],
    /// Largest corner of the finest level's bounds, object space.
    pub bounds_max: [f32; 3],
    /// Triangles of every part's first level.
    pub triangles_lod0: usize,
    /// `extras.lod_screen_size` of the LOD nodes, by glTF level (the fraction of the screen height below which
    /// the next level takes over), in level order.
    pub lod_screen_sizes: Vec<(u32, f32)>,
    /// `extras.shadow_lod` of the model's nodes (the level to cast shadows from), when one names it.
    pub shadow_lod: Option<u32>,
}

const GLB_MAGIC: u32 = 0x4654_6C67;
const GLB_JSON: u32 = 0x4E4F_534A;
const GLB_BIN: u32 = 0x004E_4942;
/// Images decoded at once.
const DECODE_THREADS: usize = 8;
/// Deepest node hierarchy followed.
const MAX_DEPTH: usize = 64;
/// glTF `componentType` codes.
const COMPONENT_U8: u64 = 5121;
const COMPONENT_U16: u64 = 5123;
const COMPONENT_U32: u64 = 5125;
const COMPONENT_F32: u64 = 5126;

/// The parsed file: its JSON, its buffers and where relative URIs resolve.
struct Doc {
    json: Value,
    buffers: Vec<Vec<u8>>,
    dir: PathBuf,
    path: PathBuf,
}

impl Doc {
    fn err(&self, message: impl std::fmt::Display) -> GltfError {
        GltfError(format!("{}: {message}", self.path.display()))
    }

    fn list(&self, key: &str) -> &[Value] {
        self.json
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn item(&self, key: &str, index: usize) -> Result<&Value, GltfError> {
        self.list(key)
            .get(index)
            .ok_or_else(|| self.err(format!("{key}[{index}] does not exist")))
    }

    /// The bytes a buffer or image `uri` names: a base64 `data:` URI, or a file next to the glTF.
    fn read_uri(&self, uri: &str, what: &str) -> Result<Vec<u8>, GltfError> {
        if let Some(rest) = uri.strip_prefix("data:") {
            let payload = rest
                .split_once(";base64,")
                .map(|(_, payload)| payload)
                .ok_or_else(|| self.err(format!("{what}: only base64 data: URIs are supported")))?;
            return decode_base64(payload)
                .ok_or_else(|| self.err(format!("{what}: the base64 data is malformed")));
        }
        let file = self.sibling(uri)?;
        std::fs::read(&file).map_err(|e| self.err(format!("{what} {}: {e}", file.display())))
    }

    /// A file next to the glTF, refusing absolute paths, `..` and `data:` URIs.
    fn sibling(&self, uri: &str) -> Result<PathBuf, GltfError> {
        if uri.starts_with("data:") {
            return Err(self.err("a data: URI is not a file"));
        }
        let rel = Path::new(uri);
        if uri.is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(self.err(format!(
                "uri '{uri}' must be a relative path inside the model's directory"
            )));
        }
        Ok(self.dir.join(rel))
    }
}

fn usize_of(value: &Value, key: &str) -> Option<usize> {
    value.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

fn f32_of(value: &Value, key: &str, default: f32) -> f32 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .map_or(default, |v| v as f32)
}

fn floats_of<const N: usize>(value: &Value, key: &str) -> Option<[f32; N]> {
    let list = value.get(key)?.as_array()?;
    if list.len() != N {
        return None;
    }
    let mut out = [0.0f32; N];
    for (slot, v) in out.iter_mut().zip(list) {
        *slot = v.as_f64()? as f32;
    }
    Some(out)
}

fn read_u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Splits a `.glb` into its JSON text and its `BIN` chunk.
fn parse_glb<'a>(bytes: &'a [u8], path: &Path) -> Result<(&'a [u8], Option<&'a [u8]>), GltfError> {
    let bad = |m: &str| GltfError(format!("{}: {m}", path.display()));
    let version = read_u32_le(bytes, 4).ok_or_else(|| bad("truncated GLB header"))?;
    if read_u32_le(bytes, 0) != Some(GLB_MAGIC) || version != 2 {
        return Err(bad("not a version 2 GLB"));
    }
    let total = read_u32_le(bytes, 8).ok_or_else(|| bad("truncated GLB header"))? as usize;
    if total > bytes.len() {
        return Err(bad("the GLB is shorter than its header says"));
    }
    let mut at = 12usize;
    let mut json = None;
    let mut bin = None;
    while at + 8 <= total {
        let len = read_u32_le(bytes, at).ok_or_else(|| bad("truncated chunk"))? as usize;
        let kind = read_u32_le(bytes, at + 4).ok_or_else(|| bad("truncated chunk"))?;
        let start = at + 8;
        let data = start
            .checked_add(len)
            .and_then(|end| bytes.get(start..end))
            .ok_or_else(|| bad("a chunk runs past the end of the file"))?;
        match kind {
            GLB_JSON if json.is_none() => json = Some(data),
            GLB_BIN if bin.is_none() => bin = Some(data),
            _ => {}
        }
        at = start + len.div_ceil(4) * 4;
    }
    let json = json.ok_or_else(|| bad("the GLB has no JSON chunk"))?;
    Ok((json, bin))
}

fn load_doc(path: &Path) -> Result<Doc, GltfError> {
    let bytes = std::fs::read(path).map_err(|e| GltfError(format!("{}: {e}", path.display())))?;
    let (json_bytes, bin) = if bytes.starts_with(b"glTF") {
        parse_glb(&bytes, path)?
    } else {
        (bytes.as_slice(), None)
    };
    let json: Value = serde_json::from_slice(json_bytes)
        .map_err(|e| GltfError(format!("{}: not glTF JSON: {e}", path.display())))?;
    if !json.is_object() {
        return Err(GltfError(format!(
            "{}: the glTF JSON is not an object",
            path.display()
        )));
    }
    let mut doc = Doc {
        json,
        buffers: Vec::new(),
        dir: path.parent().map_or_else(PathBuf::new, Path::to_path_buf),
        path: path.to_path_buf(),
    };
    let mut buffers = Vec::new();
    for (index, buffer) in doc.list("buffers").iter().enumerate() {
        let length = usize_of(buffer, "byteLength")
            .ok_or_else(|| doc.err(format!("buffers[{index}] has no byteLength")))?;
        let data = match buffer.get("uri").and_then(Value::as_str) {
            Some(uri) => doc.read_uri(uri, &format!("buffers[{index}]"))?,
            None if index == 0 => bin
                .map(<[u8]>::to_vec)
                .ok_or_else(|| doc.err("buffers[0] has no uri and the file has no GLB BIN chunk"))?,
            None => return Err(doc.err(format!("buffers[{index}] has no uri"))),
        };
        if data.len() < length {
            return Err(doc.err(format!(
                "buffers[{index}] holds {} bytes but byteLength is {length}",
                data.len()
            )));
        }
        buffers.push(data);
    }
    doc.buffers = buffers;
    Ok(doc)
}

/// The bytes of a buffer view.
fn view_bytes(doc: &Doc, view_index: usize) -> Result<(&[u8], Option<usize>), GltfError> {
    let view = doc.item("bufferViews", view_index)?;
    let buffer = usize_of(view, "buffer")
        .ok_or_else(|| doc.err(format!("bufferViews[{view_index}] has no buffer")))?;
    let offset = usize_of(view, "byteOffset").unwrap_or(0);
    let length = usize_of(view, "byteLength")
        .ok_or_else(|| doc.err(format!("bufferViews[{view_index}] has no byteLength")))?;
    let data = doc
        .buffers
        .get(buffer)
        .and_then(|b| offset.checked_add(length).and_then(|end| b.get(offset..end)))
        .ok_or_else(|| doc.err(format!("bufferViews[{view_index}] runs past its buffer")))?;
    Ok((data, usize_of(view, "byteStride")))
}

/// An accessor's elements: `count` elements of `components` values of `component_size` bytes, `stride` apart.
struct Elements<'a> {
    data: &'a [u8],
    count: usize,
    components: usize,
    component_type: u64,
    component_size: usize,
    stride: usize,
}

fn accessor(doc: &Doc, index: usize) -> Result<Elements<'_>, GltfError> {
    let acc = doc.item("accessors", index)?;
    if acc.get("sparse").is_some() {
        return Err(doc.err(format!("accessors[{index}] is sparse (not supported)")));
    }
    let count = usize_of(acc, "count").ok_or_else(|| doc.err(format!("accessors[{index}] has no count")))?;
    let component_type = acc.get("componentType").and_then(Value::as_u64).unwrap_or(0);
    let component_size = match component_type {
        5120 | COMPONENT_U8 => 1,
        5122 | COMPONENT_U16 => 2,
        COMPONENT_U32 | COMPONENT_F32 => 4,
        other => return Err(doc.err(format!("accessors[{index}] has componentType {other}"))),
    };
    let components = match acc.get("type").and_then(Value::as_str) {
        Some("SCALAR") => 1,
        Some("VEC2") => 2,
        Some("VEC3") => 3,
        Some("VEC4") => 4,
        Some("MAT4") => 16,
        other => return Err(doc.err(format!("accessors[{index}] has type {other:?}"))),
    };
    let view = usize_of(acc, "bufferView")
        .ok_or_else(|| doc.err(format!("accessors[{index}] has no bufferView")))?;
    let (bytes, stride) = view_bytes(doc, view)?;
    let element = component_size * components;
    let stride = stride.unwrap_or(element).max(element);
    let offset = usize_of(acc, "byteOffset").unwrap_or(0);
    let needed = if count == 0 {
        Some(0)
    } else {
        stride.checked_mul(count - 1).and_then(|v| v.checked_add(element))
    };
    let data = needed
        .and_then(|needed| offset.checked_add(needed))
        .and_then(|end| bytes.get(offset..end))
        .ok_or_else(|| doc.err(format!("accessors[{index}] runs past its buffer view")))?;
    Ok(Elements {
        data,
        count,
        components,
        component_type,
        component_size,
        stride,
    })
}

/// A float accessor of `components` values per element, flattened.
fn floats(doc: &Doc, index: usize, components: usize, what: &str) -> Result<Vec<f32>, GltfError> {
    let e = accessor(doc, index)?;
    if e.component_type != COMPONENT_F32 || e.components != components {
        return Err(doc.err(format!(
            "{what} (accessors[{index}]) must be {components} floats per element (normalized integers are not supported)"
        )));
    }
    let mut out = Vec::with_capacity(e.count * components);
    for i in 0..e.count {
        for c in 0..components {
            let at = i * e.stride + c * e.component_size;
            let b = e
                .data
                .get(at..at + 4)
                .ok_or_else(|| doc.err(format!("{what}: element {i} is out of range")))?;
            let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if !v.is_finite() {
                return Err(doc.err(format!("{what}: element {i} is not finite")));
            }
            out.push(v);
        }
    }
    Ok(out)
}

fn indices(doc: &Doc, index: usize, vertex_count: usize) -> Result<Vec<u32>, GltfError> {
    let e = accessor(doc, index)?;
    if e.components != 1 {
        return Err(doc.err(format!("indices (accessors[{index}]) must be scalars")));
    }
    let mut out = Vec::with_capacity(e.count);
    for i in 0..e.count {
        let at = i * e.stride;
        let value = match e.component_type {
            COMPONENT_U8 => e.data.get(at).map(|&b| u32::from(b)),
            COMPONENT_U16 => e
                .data
                .get(at..at + 2)
                .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]]))),
            COMPONENT_U32 => e
                .data
                .get(at..at + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            other => return Err(doc.err(format!("indices (accessors[{index}]) have componentType {other}"))),
        }
        .ok_or_else(|| doc.err(format!("indices: element {i} is out of range")))?;
        if value as usize >= vertex_count {
            return Err(doc.err(format!("index {value} is past the {vertex_count} vertices")));
        }
        out.push(value);
    }
    Ok(out)
}

/// The `_LOD<n>` suffix of a node name (case-insensitive), or `None`.
fn lod_suffix(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let at = lower.rfind("_lod")?;
    let digits = &lower[at + 4..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn local_matrix(node: &Value) -> Mat4 {
    if let Some(m) = floats_of::<16>(node, "matrix") {
        return Mat4::from_cols_array(&m);
    }
    let t = floats_of::<3>(node, "translation").unwrap_or([0.0; 3]);
    let r = floats_of::<4>(node, "rotation").unwrap_or([0.0, 0.0, 0.0, 1.0]);
    let s = floats_of::<3>(node, "scale").unwrap_or([1.0; 3]);
    let rotation = Quat::from_xyzw(r[0], r[1], r[2], r[3]).normalize();
    let rotation = if rotation.is_finite() {
        rotation
    } else {
        Quat::IDENTITY
    };
    Mat4::from_scale_rotation_translation(Vec3::from(s), rotation, Vec3::from(t))
}

/// A mesh placed by a node: the mesh, its world matrix and its level of detail.
struct Placed {
    mesh: usize,
    world: Mat4,
    lod: u32,
}

/// What the default scene places, and the vegetation extras its nodes carry.
struct Placement {
    placed: Vec<Placed>,
    lod_screen_sizes: Vec<(u32, f32)>,
    shadow_lod: Option<u32>,
}

fn is_billboard(name: Option<&str>) -> bool {
    name.is_some_and(|n| n.to_ascii_lowercase().contains("billboard"))
}

/// Every mesh the default scene places, depth first.
fn placed_meshes(doc: &Doc) -> Result<Placement, GltfError> {
    let nodes = doc.list("nodes");
    let scene_index = usize_of(&doc.json, "scene").unwrap_or(0);
    let roots: Vec<usize> = match doc.list("scenes").get(scene_index).and_then(|s| s.get("nodes")) {
        Some(list) => list
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).map(|v| v as usize).collect())
            .unwrap_or_default(),
        None => {
            // No scene: every node no other node names as a child.
            let mut child = vec![false; nodes.len()];
            for node in nodes {
                for c in node
                    .get("children")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(slot) = c.as_u64().and_then(|c| child.get_mut(c as usize)) {
                        *slot = true;
                    }
                }
            }
            (0..nodes.len()).filter(|&i| !child[i]).collect()
        }
    };
    let mut visited = vec![false; nodes.len()];
    let mut out = Vec::new();
    let mut lod_screen_sizes: Vec<(u32, f32)> = Vec::new();
    let mut shadow_lod = None;
    let mut stack: Vec<(usize, Mat4, usize)> =
        roots.into_iter().rev().map(|r| (r, Mat4::IDENTITY, 0)).collect();
    while let Some((index, parent, depth)) = stack.pop() {
        let node = nodes
            .get(index)
            .ok_or_else(|| doc.err(format!("nodes[{index}] does not exist")))?;
        // A node reached twice (a cycle, or a root listed again as a child) is placed once.
        if std::mem::replace(&mut visited[index], true) {
            continue;
        }
        if depth > MAX_DEPTH {
            return Err(doc.err("the node hierarchy is deeper than 64"));
        }
        let world = parent * local_matrix(node);
        let name = node.get("name").and_then(Value::as_str);
        let level = name.and_then(lod_suffix);
        let extras = node.get("extras");
        if let Some(lod) = extras.and_then(|e| e.get("shadow_lod")).and_then(Value::as_u64) {
            shadow_lod = Some(u32::try_from(lod).unwrap_or(u32::MAX));
        }
        if let Some(mesh) = usize_of(node, "mesh") {
            let mesh_name = doc.item("meshes", mesh)?.get("name").and_then(Value::as_str);
            // A billboard the chain names (`<stem>_LOD3`) is a level; an unnamed one is an impostor card.
            if level.is_some() || (!is_billboard(name) && !is_billboard(mesh_name)) {
                let lod = level.unwrap_or(0);
                if let Some(size) = extras
                    .and_then(|e| e.get("lod_screen_size"))
                    .and_then(Value::as_f64)
                    .map(|v| v as f32)
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    && !lod_screen_sizes.iter().any(|(l, _)| *l == lod)
                {
                    lod_screen_sizes.push((lod, size));
                }
                out.push(Placed { mesh, world, lod });
            }
        }
        let children: Vec<usize> = node
            .get("children")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_u64).map(|v| v as usize).collect())
            .unwrap_or_default();
        for child in children.into_iter().rev() {
            stack.push((child, world, depth + 1));
        }
    }
    lod_screen_sizes.sort_by_key(|(lod, _)| *lod);
    Ok(Placement {
        placed: out,
        lod_screen_sizes,
        shadow_lod,
    })
}

/// Appends one primitive, transformed by `world`, to `lod`.
fn append_primitive(doc: &Doc, primitive: &Value, world: Mat4, lod: &mut PartLod) -> Result<(), GltfError> {
    let attrs = primitive
        .get("attributes")
        .ok_or_else(|| doc.err("a primitive has no attributes"))?;
    let position = usize_of(attrs, "POSITION").ok_or_else(|| doc.err("a primitive has no POSITION"))?;
    let positions = floats(doc, position, 3, "POSITION")?;
    let count = positions.len() / 3;
    let normals = match usize_of(attrs, "NORMAL") {
        Some(i) => Some(floats(doc, i, 3, "NORMAL")?),
        None => None,
    };
    let uvs = match usize_of(attrs, "TEXCOORD_0") {
        Some(i) => Some(floats(doc, i, 2, "TEXCOORD_0")?),
        None => None,
    };
    if normals.as_ref().is_some_and(|n| n.len() != count * 3)
        || uvs.as_ref().is_some_and(|u| u.len() != count * 2)
    {
        return Err(doc.err("a primitive's attributes have different counts"));
    }
    let mut tris = match usize_of(primitive, "indices") {
        Some(i) => indices(doc, i, count)?,
        None => (0..count as u32).collect(),
    };
    tris.truncate(tris.len() / 3 * 3);
    // A mirroring transform flips the winding.
    if world.determinant() < 0.0 {
        for tri in tris.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
    }
    let normal_matrix = {
        let m = Mat3::from_mat4(world);
        if m.determinant().abs() > 1e-12 {
            m.inverse().transpose()
        } else {
            m
        }
    };
    let base = u32::try_from(lod.vertices.len())
        .ok()
        .filter(|b| b.checked_add(count as u32).is_some())
        .ok_or_else(|| doc.err("more than 4 billion vertices"))?;
    let first = lod.vertices.len();
    for i in 0..count {
        let p = world.transform_point3(Vec3::new(
            positions[i * 3],
            positions[i * 3 + 1],
            positions[i * 3 + 2],
        ));
        let n = normals.as_ref().map_or(Vec3::ZERO, |n| {
            (normal_matrix * Vec3::new(n[i * 3], n[i * 3 + 1], n[i * 3 + 2])).normalize_or_zero()
        });
        let uv = uvs.as_ref().map_or([0.0, 0.0], |u| [u[i * 2], u[i * 2 + 1]]);
        lod.vertices
            .push(LabVertex::from_pos_normal_uv(p.to_array(), n.to_array(), uv));
    }
    if normals.is_none() {
        let pos: Vec<[f32; 3]> = lod.vertices[first..].iter().map(|v| v.position).collect();
        let mut generated = vec![[0.0f32, 1.0, 0.0]; pos.len()];
        if regia_render::generate_vertex_normals(&pos, &tris, &mut generated) {
            for (v, n) in lod.vertices[first..].iter_mut().zip(generated) {
                v.normal = n;
            }
        }
    }
    lod.indices.extend(tris.iter().map(|&i| i + base));
    Ok(())
}

/// Per-vertex tangents (MikkTSpace through the engine's generator; any frame where it fails).
fn fill_tangents(lod: &mut PartLod) {
    let positions: Vec<[f32; 3]> = lod.vertices.iter().map(|v| v.position).collect();
    let normals: Vec<[f32; 3]> = lod.vertices.iter().map(|v| v.normal).collect();
    let uvs: Vec<[f32; 2]> = lod.vertices.iter().map(|v| v.uv).collect();
    let mut tangents = vec![[0.0f32; 4]; lod.vertices.len()];
    let ok = regia_render::generate_vertex_tangents(&positions, &normals, &uvs, &lod.indices, &mut tangents);
    for (v, t) in lod.vertices.iter_mut().zip(&tangents) {
        if ok && (t[0] != 0.0 || t[1] != 0.0 || t[2] != 0.0) {
            v.tangent = *t;
        } else {
            let n = Vec3::from_array(v.normal).normalize_or_zero();
            let a = if n == Vec3::ZERO {
                Vec3::X
            } else {
                n.any_orthonormal_vector()
            };
            v.tangent = [a.x, a.y, a.z, 1.0];
        }
    }
}

/// The image index a `textureInfo` names.
fn texture_source(doc: &Doc, info: Option<&Value>) -> Option<usize> {
    let texture = usize_of(info?, "index")?;
    usize_of(doc.list("textures").get(texture)?, "source")
}

/// Decodes one image to RGBA8, at most `max` texels on its longer edge (`0` = no cap).
/// Decodes standard base64 (RFC 4648 § 4, `=` padding optional, no whitespace). `None` on a bad character.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in bytes {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn decode_image(doc: &Doc, index: usize, max: u32) -> Result<LabTexture, GltfError> {
    let image = doc.item("images", index)?;
    let bytes: std::borrow::Cow<'_, [u8]> = match image.get("uri").and_then(Value::as_str) {
        Some(uri) => std::borrow::Cow::Owned(doc.read_uri(uri, &format!("images[{index}]"))?),
        None => {
            let view = usize_of(image, "bufferView")
                .ok_or_else(|| doc.err(format!("images[{index}] has no uri or bufferView")))?;
            std::borrow::Cow::Borrowed(view_bytes(doc, view)?.0)
        }
    };
    let decoded = image::load_from_memory(&bytes).map_err(|e| doc.err(format!("images[{index}]: {e}")))?;
    let mut rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    let longest = w.max(h);
    if max > 0 && longest > max {
        let scale = max as f32 / longest as f32;
        let nw = ((w as f32 * scale).round() as u32).clamp(1, max);
        let nh = ((h as f32 * scale).round() as u32).clamp(1, max);
        rgba = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
    }
    let (width, height) = rgba.dimensions();
    Ok(LabTexture {
        width,
        height,
        rgba: rgba.into_raw(),
    })
}

/// Decodes `wanted` images, `DECODE_THREADS` at a time.
fn decode_images(doc: &Doc, wanted: &[usize], max: u32) -> Result<BTreeMap<usize, LabTexture>, GltfError> {
    let mut out = BTreeMap::new();
    for batch in wanted.chunks(DECODE_THREADS) {
        let results: Vec<Result<LabTexture, GltfError>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|&i| s.spawn(move || decode_image(doc, i, max)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(doc.err("an image decoder thread panicked")))
                })
                .collect()
        });
        for (&index, result) in batch.iter().zip(results) {
            out.insert(index, result?);
        }
    }
    Ok(out)
}

/// The material slots of one glTF material: (base colour, normal, occlusion, metallic-roughness, emissive).
fn material_sources(doc: &Doc, material: &Value) -> [Option<usize>; 5] {
    let pbr = material.get("pbrMetallicRoughness");
    [
        texture_source(doc, pbr.and_then(|p| p.get("baseColorTexture"))),
        texture_source(doc, material.get("normalTexture")),
        texture_source(doc, material.get("occlusionTexture")),
        texture_source(doc, pbr.and_then(|p| p.get("metallicRoughnessTexture"))),
        texture_source(doc, material.get("emissiveTexture")),
    ]
}

/// A packed ORM image (R occlusion, G roughness, B metallic) from separate occlusion and metallic-roughness
/// images; `None` occlusion keeps R at 1.
fn pack_orm(occlusion: Option<&LabTexture>, mr: &LabTexture) -> LabTexture {
    let mut out = mr.clone();
    let same = occlusion.filter(|o| o.width == mr.width && o.height == mr.height);
    for (i, px) in out.rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px[0] = same.and_then(|o| o.rgba.get(i * 4).copied()).unwrap_or(255);
    }
    out
}

fn build_material(doc: &Doc, index: Option<usize>, images: &BTreeMap<usize, LabTexture>) -> CpuMaterial {
    let mut m = CpuMaterial::default();
    let Some(material) = index.and_then(|i| doc.list("materials").get(i)) else {
        return m;
    };
    let pbr = material
        .get("pbrMetallicRoughness")
        .cloned()
        .unwrap_or(Value::Null);
    m.base_color = floats_of::<4>(&pbr, "baseColorFactor").unwrap_or([1.0; 4]);
    m.metallic = f32_of(&pbr, "metallicFactor", 1.0).clamp(0.0, 1.0);
    m.roughness = f32_of(&pbr, "roughnessFactor", 1.0).clamp(0.0, 1.0);
    m.emissive = floats_of::<3>(material, "emissiveFactor").unwrap_or([0.0; 3]);
    m.double_sided = material
        .get("doubleSided")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Blended foliage is drawn alpha-tested: thousands of sorted instances would cost too much.
    if let Some("MASK" | "BLEND") = material.get("alphaMode").and_then(Value::as_str) {
        m.alpha_mode = ALPHA_MASK;
        m.alpha_cutoff = f32_of(material, "alphaCutoff", 0.5);
    }
    let [albedo, normal, occlusion, mr, emissive] = material_sources(doc, material);
    let get = |i: Option<usize>| i.and_then(|i| images.get(&i));
    m.albedo = get(albedo).cloned();
    if let Some(n) = get(normal) {
        m.normal_map = Some(n.clone());
        m.normal_scale = material
            .get("normalTexture")
            .map_or(1.0, |t| f32_of(t, "scale", 1.0));
    }
    m.occlusion_strength = material
        .get("occlusionTexture")
        .map_or(1.0, |t| f32_of(t, "strength", 1.0));
    match (get(occlusion), get(mr)) {
        (Some(o), Some(_)) if occlusion == mr => {
            m.occlusion_map = Some(o.clone());
            m.orm_packed = true;
        }
        (o, Some(r)) => {
            m.occlusion_map = Some(pack_orm(o, r));
            m.orm_packed = true;
        }
        (Some(o), None) => m.occlusion_map = Some(o.clone()),
        (None, None) => {}
    }
    m.emissive_map = get(emissive).cloned();
    m
}

/// Reads a `.gltf` or `.glb` model; textures are scaled down to at most `texture_max` texels on their longer edge
/// (`0` keeps the authored size).
///
/// # Errors
/// A [`GltfError`] naming the file and the problem: unreadable files, malformed JSON or GLB, accessors or buffer
/// views out of range, unsupported attribute formats, undecodable images, or no triangles at all.
pub fn load_model(path: &Path, texture_max: u32) -> Result<Model, GltfError> {
    let _span = tracing::info_span!("showcase_gltf_load").entered();
    let doc = load_doc(path)?;
    let Placement {
        placed,
        lod_screen_sizes,
        shadow_lod,
    } = placed_meshes(&doc)?;

    // (material, glTF level) -> merged triangles.
    let mut groups: BTreeMap<(Option<usize>, u32), PartLod> = BTreeMap::new();
    for item in &placed {
        let mesh = doc.item("meshes", item.mesh)?;
        for primitive in mesh
            .get("primitives")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if primitive.get("mode").and_then(Value::as_u64).unwrap_or(4) != 4 {
                continue;
            }
            let material = usize_of(primitive, "material");
            let lod = groups.entry((material, item.lod)).or_insert_with(|| PartLod {
                vertices: Vec::new(),
                indices: Vec::new(),
                source_lod: item.lod,
            });
            append_primitive(&doc, primitive, item.world, lod)?;
        }
    }
    groups.retain(|_, lod| !lod.indices.is_empty());
    if groups.is_empty() {
        return Err(doc.err("the default scene places no triangles"));
    }

    // Images the used materials name, decoded once each.
    let mut wanted: Vec<usize> = Vec::new();
    for &(material, _) in groups.keys() {
        if let Some(m) = material.and_then(|i| doc.list("materials").get(i)) {
            wanted.extend(material_sources(&doc, m).into_iter().flatten());
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    let images = decode_images(&doc, &wanted, texture_max)?;

    let finest = groups.keys().map(|&(_, lod)| lod).min().unwrap_or(0);
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    let mut parts: Vec<ModelPart> = Vec::new();
    let mut current: Option<Option<usize>> = None;
    for ((material, _), mut lod) in groups {
        fill_tangents(&mut lod);
        if lod.source_lod == finest {
            for v in &lod.vertices {
                for k in 0..3 {
                    lo[k] = lo[k].min(v.position[k]);
                    hi[k] = hi[k].max(v.position[k]);
                }
            }
        }
        if current != Some(material) {
            current = Some(material);
            parts.push(ModelPart {
                material: build_material(&doc, material, &images),
                lods: Vec::new(),
            });
        }
        if let Some(part) = parts.last_mut() {
            part.lods.push(lod);
        }
    }
    if !lo.iter().all(|v| v.is_finite()) {
        (lo, hi) = ([0.0; 3], [0.0; 3]);
    }
    let triangles_lod0 = parts
        .iter()
        .filter_map(|p| p.lods.first())
        .filter(|l| l.source_lod == finest)
        .map(|l| l.indices.len() / 3)
        .sum();
    Ok(Model {
        parts,
        bounds_min: lo,
        bounds_max: hi,
        triangles_lod0,
        lod_screen_sizes,
        shadow_lod,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use regia_test_utils::unique_temp_dir;
    use serde_json::json;

    fn write_png(path: &Path, width: u32, height: u32) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..width * height)
            .flat_map(|i| [(i % 251) as u8, 128, 64, 255])
            .collect();
        writer.write_image_data(&data).unwrap();
    }

    /// A vertical unit quad (two triangles) with normals, UVs and u16 indices.
    fn quad_bin() -> Vec<u8> {
        let mut bin = Vec::new();
        for p in [
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ] {
            for v in p {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        for _ in 0..4 {
            for v in [0.0f32, 0.0, 1.0] {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        for uv in [[0.0f32, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]] {
            for v in uv {
                bin.extend_from_slice(&v.to_le_bytes());
            }
        }
        for i in [0u16, 1, 2, 0, 2, 3] {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        bin
    }

    fn quad_json(buffer_uri: Option<&str>, len: usize) -> Value {
        let mut buffer = json!({ "byteLength": len });
        if let Some(uri) = buffer_uri {
            buffer["uri"] = json!(uri);
        }
        json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "name": "quad_root", "children": [1, 2, 3], "translation": [0.0, 1.0, 0.0], "scale": [2.0, 2.0, 2.0] },
                { "name": "quad_LOD0", "mesh": 0 },
                { "name": "quad_LOD1", "mesh": 0 },
                { "name": "quad_billboard", "mesh": 0 }
            ],
            "meshes": [{ "primitives": [{
                "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
                "indices": 3,
                "material": 0
            }] }],
            "materials": [{
                "pbrMetallicRoughness": {
                    "baseColorTexture": { "index": 0 },
                    "metallicRoughnessTexture": { "index": 1 },
                    "roughnessFactor": 0.8
                },
                "occlusionTexture": { "index": 1 },
                "alphaMode": "MASK",
                "doubleSided": true
            }],
            "textures": [{ "source": 0 }, { "source": 1 }],
            "images": [{ "uri": "albedo.png" }, { "uri": "arm.png" }],
            "buffers": [buffer],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": 48 },
                { "buffer": 0, "byteOffset": 48, "byteLength": 48 },
                { "buffer": 0, "byteOffset": 96, "byteLength": 32 },
                { "buffer": 0, "byteOffset": 128, "byteLength": 12 }
            ],
            "accessors": [
                { "bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3" },
                { "bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC3" },
                { "bufferView": 2, "componentType": 5126, "count": 4, "type": "VEC2" },
                { "bufferView": 3, "componentType": 5123, "count": 6, "type": "SCALAR" }
            ]
        })
    }

    fn glb(json: &Value, bin: &[u8]) -> Vec<u8> {
        let mut text = serde_json::to_vec(json).unwrap();
        while !text.len().is_multiple_of(4) {
            text.push(b' ');
        }
        let mut bin = bin.to_vec();
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let total = 12 + 8 + text.len() + 8 + bin.len();
        let mut out = Vec::new();
        out.extend_from_slice(&GLB_MAGIC.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(&GLB_JSON.to_le_bytes());
        out.extend_from_slice(&text);
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(&GLB_BIN.to_le_bytes());
        out.extend_from_slice(&bin);
        out
    }

    fn scene_dir() -> PathBuf {
        let dir = unique_temp_dir("showcase-gltf");
        let bin = quad_bin();
        std::fs::write(dir.join("quad.bin"), &bin).unwrap();
        std::fs::write(
            dir.join("quad.gltf"),
            serde_json::to_vec(&quad_json(Some("quad.bin"), bin.len())).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("quad.glb"), glb(&quad_json(None, bin.len()), &bin)).unwrap();
        write_png(&dir.join("albedo.png"), 64, 32);
        write_png(&dir.join("arm.png"), 8, 8);
        dir
    }

    fn check(model: &Model) {
        assert_eq!(model.parts.len(), 1, "one material, one part");
        let part = &model.parts[0];
        assert_eq!(part.lods.len(), 2, "LOD0 and LOD1; the billboard node is skipped");
        assert_eq!(part.lods[0].source_lod, 0);
        assert_eq!(part.lods[1].source_lod, 1);
        for lod in &part.lods {
            assert_eq!((lod.vertices.len(), lod.indices.len()), (4, 6));
        }
        assert_eq!(model.triangles_lod0, 2);
        // Root scale 2 and lift 1 m.
        let close = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5);
        assert!(close(model.bounds_min, [0.0, 1.0, 0.0]), "{:?}", model.bounds_min);
        assert!(close(model.bounds_max, [2.0, 3.0, 0.0]), "{:?}", model.bounds_max);
        let m = &part.material;
        let albedo = m.albedo.as_ref().expect("albedo");
        assert_eq!(
            (albedo.width, albedo.height),
            (16, 8),
            "capped to 16 on the longer edge"
        );
        assert!(
            m.orm_packed && m.occlusion_map.is_some(),
            "one ARM image: packed ORM"
        );
        assert_eq!(m.normal_scale, 0.0, "no normal map");
        assert_eq!(m.alpha_mode, ALPHA_MASK);
        assert!(m.double_sided);
        assert!((m.roughness - 0.8).abs() < 1e-6);
        let n = part.lods[0].vertices[0].normal;
        assert!(
            (n[2] - 1.0).abs() < 1e-5,
            "normal kept under a uniform scale: {n:?}"
        );
    }

    #[test]
    fn a_gltf_and_its_glb_read_the_same() {
        let dir = scene_dir();
        check(&load_model(&dir.join("quad.gltf"), 16).unwrap());
        check(&load_model(&dir.join("quad.glb"), 16).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lod_suffixes_are_read() {
        assert_eq!(lod_suffix("tree_beech_s1_LOD2"), Some(2));
        assert_eq!(lod_suffix("rock_09_lod0"), Some(0));
        assert_eq!(lod_suffix("rock_09"), None);
        assert_eq!(lod_suffix("x_LOD"), None);
        assert_eq!(lod_suffix("x_LODa"), None);
    }

    #[test]
    fn malformed_files_are_errors_not_panics() {
        let dir = scene_dir();
        let bin = quad_bin();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("garbage.gltf", b"not json at all".to_vec()),
            ("empty.gltf", Vec::new()),
            ("short.glb", glb(&quad_json(None, bin.len()), &bin)[..20].to_vec()),
            ("nobin.glb", glb(&quad_json(None, bin.len()), &[])),
            (
                "big.gltf",
                serde_json::to_vec(&quad_json(Some("quad.bin"), bin.len() + 100)).unwrap(),
            ),
            (
                "escape.gltf",
                serde_json::to_vec(&quad_json(Some("../quad.bin"), bin.len())).unwrap(),
            ),
            (
                "data_short.gltf",
                serde_json::to_vec(&quad_json(Some("data:application/octet-stream;base64,AAAA"), 3)).unwrap(),
            ),
        ];
        for (name, bytes) in cases {
            std::fs::write(dir.join(name), bytes).unwrap();
            assert!(load_model(&dir.join(name), 0).is_err(), "{name} is refused");
        }
        // An accessor past its view and an index past the vertices.
        let mut doc = quad_json(Some("quad.bin"), bin.len());
        doc["accessors"][0]["count"] = json!(400);
        std::fs::write(dir.join("count.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();
        assert!(load_model(&dir.join("count.gltf"), 0).is_err());
        let mut doc = quad_json(Some("quad.bin"), bin.len());
        doc["accessors"][3]["count"] = json!(5);
        doc["accessors"][0]["count"] = json!(2);
        doc["accessors"][1]["count"] = json!(2);
        doc["accessors"][2]["count"] = json!(2);
        std::fs::write(dir.join("index.gltf"), serde_json::to_vec(&doc).unwrap()).unwrap();
        assert!(load_model(&dir.join("index.gltf"), 0).is_err());
        assert!(load_model(&dir.join("missing.gltf"), 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod base64_tests {
    #[test]
    fn base64_decodes_with_and_without_padding() {
        assert_eq!(super::decode_base64("TWFu").as_deref(), Some(&b"Man"[..]));
        assert_eq!(super::decode_base64("TWE=").as_deref(), Some(&b"Ma"[..]));
        assert_eq!(super::decode_base64("TQ").as_deref(), Some(&b"M"[..]));
        assert_eq!(super::decode_base64("AAAA").as_deref(), Some(&[0u8, 0, 0][..]));
        assert_eq!(super::decode_base64("TW!u"), None);
    }
}
