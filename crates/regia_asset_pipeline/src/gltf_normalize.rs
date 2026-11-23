// SPDX-License-Identifier: MPL-2.0
//! Turns a glTF that uses a data-layout extension into one the readers already understand
//! (`docs/formats/gltf-extensions.md`, ADR-3016).
//!
//! Every glTF path of the pipeline (cook, tools, scene export, editor viewport) reads accessors
//! through the `gltf` crate, which reads floats for positions, normals, tangents and morph deltas and
//! reaches an `unreachable!` on a texture coordinate or color in another component type. Rather than
//! teach each reader, this module rewrites the document once, right after the file is opened:
//!
//! - **`EXT_meshopt_compression`**: every compressed view is decoded into the buffer the view points
//!   at. A buffer marked `fallback` is never read from the file.
//! - **`KHR_mesh_quantization`**: every integer attribute becomes a float accessor in one new buffer.
//!   A quantized UV set whose material scales it back with `KHR_texture_transform` gets that transform
//!   baked in, because the renderer has no UV transform.
//! - **`EXT_mesh_gpu_instancing`**: the transforms are read out of their accessors.
//! - **`EXT_texture_webp`**: the texture's `source` points at the WebP image.
//! - **`extensionsRequired`**: an extension the importer cannot read is the error
//!   `gltf_ext_unsupported`. The `gltf` crate's own check is cleared, since it only knows its cargo
//!   features.
//!
//! A file that uses none of these keeps its document and buffers untouched, so it cooks byte for byte
//! as before.

use std::collections::{BTreeMap, BTreeSet};

use gltf::json::Root;
use gltf::json::accessor::{ComponentType, GenericComponentType};
use gltf::json::validation::{Checked, USize64};
use regia_core::math::{Mat4, Quat, Vec3};
use serde_json::Value;

use crate::ImportError;
use crate::gltf_extensions::TextureTransform;
use crate::gltf_meshopt::{MeshoptView, decode_stream};

/// Agent error code: the file needs an extension the importer cannot read.
pub const GLTF_EXT_UNSUPPORTED: &str = "gltf_ext_unsupported";
/// Agent error code: an `EXT_meshopt_compression` view is broken.
pub const GLTF_MESHOPT_INVALID: &str = "gltf_meshopt_invalid";
/// Agent error code: an integer attribute uses a component type nothing allows.
pub const GLTF_QUANTIZATION_INVALID: &str = "gltf_quantization_invalid";
/// Agent error code: an `EXT_mesh_gpu_instancing` block is broken.
pub const GLTF_INSTANCING_INVALID: &str = "gltf_instancing_invalid";

const MESHOPT: &str = "EXT_meshopt_compression";
const QUANTIZATION: &str = "KHR_mesh_quantization";
const INSTANCING: &str = "EXT_mesh_gpu_instancing";
const WEBP: &str = "EXT_texture_webp";

/// Most elements an accessor without a buffer view may declare (it is all zeros and sparse edits, so
/// nothing in the file bounds it).
const MAX_VIEWLESS_ELEMENTS: u64 = 1 << 24;

/// Most bytes of floats one file's integer attributes may become. Many accessors can name one view, so
/// the size of the file does not bound this on its own.
const MAX_NORMALIZED_BYTES: u64 = 1 << 32;

/// Most bytes all of a file's compressed views may decode to. One stream can back many views.
const MAX_TOTAL_DECODED_BYTES: u64 = 1 << 34;

/// Most instances one file may hold, over every node (and every visit of a node shared by parents).
pub(crate) const MAX_FILE_INSTANCES: usize = 1 << 24;

/// What the importer does with an extension a file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSupport {
    /// The data reaches the cooked output.
    Cooked,
    /// The data is read into [`crate::GltfImportResult::extensions`] and stops short of the cooked
    /// output: the renderer has no parameter for it yet, or (`KHR_lights_punctual`) only the editor's
    /// named hierarchy uses it.
    SideTable,
}

impl ExtensionSupport {
    /// Stable JSON name: `cooked` or `side_table`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cooked => "cooked",
            Self::SideTable => "side_table",
        }
    }
}

/// Extensions the importer reads, in catalog order.
const KNOWN_EXTENSIONS: &[(&str, ExtensionSupport)] = &[
    ("KHR_mesh_quantization", ExtensionSupport::Cooked),
    ("EXT_meshopt_compression", ExtensionSupport::Cooked),
    ("EXT_mesh_gpu_instancing", ExtensionSupport::Cooked),
    ("EXT_texture_webp", ExtensionSupport::Cooked),
    ("KHR_materials_variants", ExtensionSupport::Cooked),
    ("KHR_materials_clearcoat", ExtensionSupport::Cooked),
    ("KHR_materials_anisotropy", ExtensionSupport::Cooked),
    ("KHR_materials_emissive_strength", ExtensionSupport::Cooked),
    ("KHR_materials_iridescence", ExtensionSupport::Cooked),
    ("KHR_materials_ior", ExtensionSupport::Cooked),
    ("KHR_materials_transmission", ExtensionSupport::Cooked),
    ("KHR_materials_volume", ExtensionSupport::Cooked),
    ("REGIA_materials_detail", ExtensionSupport::Cooked),
    ("KHR_materials_sheen", ExtensionSupport::SideTable),
    ("KHR_materials_specular", ExtensionSupport::SideTable),
    ("KHR_texture_transform", ExtensionSupport::SideTable),
    ("KHR_lights_punctual", ExtensionSupport::SideTable),
];

/// Whether the importer can take a file that requires `extension`, and what it does with it. `None`
/// means it cannot: a required one is the error `gltf_ext_unsupported`, an optional one falls back
/// with a warning.
#[must_use]
pub fn extension_support(extension: &str) -> Option<ExtensionSupport> {
    KNOWN_EXTENSIONS
        .iter()
        .find(|(name, _)| *name == extension)
        .map(|(_, support)| *support)
}

/// The importer capability table. Material coverage and the content-lint catalog
/// report this list instead of a second copy.
#[must_use]
pub fn known_extension_support() -> &'static [(&'static str, ExtensionSupport)] {
    KNOWN_EXTENSIONS
}

fn extension_error(code: &'static str, extension: &str, message: String) -> ImportError {
    ImportError::GltfExtension {
        code,
        extension: extension.to_string(),
        message,
    }
}

fn unsupported(extension: &str, why: &str) -> ImportError {
    // The name comes from the file; keep the message a sane length.
    let name: String = extension.chars().take(128).collect();
    let message = format!("glTF extension `{name}` {why}");
    extension_error(GLTF_EXT_UNSUPPORTED, &name, message)
}

/// The JSON of a `.gltf` or a `.glb`, `Null` when it is not readable (the `gltf` parser reports why).
pub(crate) fn raw_json(bytes: &[u8]) -> Value {
    crate::gltf_extensions::json_chunk(bytes)
        .and_then(|json| serde_json::from_slice(json).ok())
        .unwrap_or(Value::Null)
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Applies the extension policy to a parsed root before the `gltf` crate validates it.
pub(crate) fn prepare_root(json: &mut Root, raw: &Value) -> Result<(), ImportError> {
    if let Some(name) = json
        .extensions_required
        .iter()
        .find(|name| extension_support(name).is_none())
    {
        return Err(unsupported(
            name,
            "is required by the file and the importer cannot read it",
        ));
    }
    json.extensions_required.clear();
    prepare_textures(json, raw)?;
    refuse_primitives_without_data(raw)
}

/// Whether the file declares `KHR_mesh_quantization` in its `extensionsUsed` or `extensionsRequired`
/// list. Read before [`prepare_root`] clears the required list. Only a declaring file may store a
/// `TEXCOORD_n` as `BYTE` or `SHORT`; [`normalize`] turns it into floats before any reader (ADR-5958).
pub(crate) fn declares_mesh_quantization(json: &Root) -> bool {
    json.extensions_used
        .iter()
        .chain(&json.extensions_required)
        .any(|name| name == QUANTIZATION)
}

/// `EXT_texture_webp` gives the texture its image. A texture left without any image cannot fall back,
/// so the extension that was meant to provide it is named (`KHR_texture_basisu` without a `source`).
fn prepare_textures(json: &mut Root, raw: &Value) -> Result<(), ImportError> {
    for (i, texture) in array(raw, "textures").iter().enumerate() {
        let extensions = texture.get("extensions").and_then(Value::as_object);
        if let Some(block) = extensions.and_then(|e| e.get(WEBP)) {
            let source = block
                .get("source")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    unsupported(
                        WEBP,
                        &format!("on texture {i} has no usable `source` image index"),
                    )
                })?;
            if let Some(typed) = json.textures.get_mut(i) {
                typed.source = gltf::json::Index::new(source);
            }
        } else if texture.get("source").is_none()
            && let Some(name) = extensions.and_then(|e| e.keys().next())
        {
            return Err(unsupported(
                name,
                &format!("is the only image of texture {i} and the importer cannot read it"),
            ));
        }
    }
    Ok(())
}

/// A primitive whose positions only exist in an extension's stream (`KHR_draco_mesh_compression` with
/// no fallback accessors) has nothing to fall back on, optional or not.
fn refuse_primitives_without_data(raw: &Value) -> Result<(), ImportError> {
    let accessors = array(raw, "accessors");
    for (m, mesh) in array(raw, "meshes").iter().enumerate() {
        for (p, primitive) in array(mesh, "primitives").iter().enumerate() {
            let Some(extensions) = primitive.get("extensions").and_then(Value::as_object) else {
                continue;
            };
            let name = extensions
                .keys()
                .find(|name| extension_support(name.as_str()).is_none());
            let Some(name) = name else { continue };
            let no_data = primitive
                .pointer("/attributes/POSITION")
                .and_then(Value::as_u64)
                .and_then(|i| accessors.get(usize::try_from(i).ok()?))
                .is_some_and(|a| a.get("bufferView").is_none() && a.get("sparse").is_none());
            if no_data {
                return Err(unsupported(
                    name,
                    &format!(
                        "holds the only vertex data of mesh {m} primitive {p} and the importer cannot read it"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Whether buffer `index` is marked as the placeholder of decoded data (it is not read from the file).
pub(crate) fn is_fallback_buffer(raw: &Value, index: usize) -> bool {
    array(raw, "buffers")
        .get(index)
        .and_then(|b| b.pointer("/extensions/EXT_meshopt_compression/fallback"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// What the extensions gave besides the rewritten document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GltfLoadData {
    /// `EXT_mesh_gpu_instancing`: per node index, one local matrix per instance (column-major).
    pub instances: BTreeMap<usize, Vec<[f32; 16]>>,
    /// Compressed buffer views decoded.
    pub meshopt_views: usize,
    /// Integer attribute accessors turned into floats.
    pub quantized_accessors: usize,
    /// Materials whose `KHR_texture_transform` is already in the vertices' UVs, because they scale a
    /// quantized UV set. The file's block still says what it said, so a reader of the raw JSON or of
    /// [`crate::gltf_extensions::GltfExtensionTable`] must not apply it a second time.
    pub baked_uv_materials: BTreeSet<usize>,
}

/// Most copies of one instanced node a tool that bakes instances into one mesh writes (the editor's
/// merged model, the scene export). The cook never bakes them.
pub const MAX_BAKED_INSTANCES: usize = 4096;

impl GltfLoadData {
    /// The matrices to bake node `node` at when a tool flattens instances into one mesh: `world * I` for
    /// each instance (up to [`MAX_BAKED_INSTANCES`]), or `world` for a node without instances. Also
    /// returns how many instances were left out.
    #[must_use]
    pub fn baked_worlds(&self, node: usize, world: Mat4) -> (Vec<Mat4>, usize) {
        match self.instances.get(&node) {
            Some(locals) => (
                locals
                    .iter()
                    .take(MAX_BAKED_INSTANCES)
                    .map(|local| world * Mat4::from_cols_array(local))
                    .collect(),
                locals.len().saturating_sub(MAX_BAKED_INSTANCES),
            ),
            None => (vec![world], 0),
        }
    }
}

/// The rewritten file.
pub(crate) struct Normalized {
    pub document: gltf::Document,
    pub buffers: Vec<Vec<u8>>,
    pub data: GltfLoadData,
}

/// Applies the data-layout extensions. See the module docs.
pub(crate) fn normalize(
    document: gltf::Document,
    raw: &Value,
    buffers: Vec<Vec<u8>>,
) -> Result<Normalized, ImportError> {
    normalize_with_checkpoint(document, raw, buffers, &mut || Ok(()))
}

/// Authoring loader variant: check cancellation between bounded extension units.
pub(crate) fn normalize_with_checkpoint(
    document: gltf::Document,
    raw: &Value,
    mut buffers: Vec<Vec<u8>>,
    checkpoint: &mut dyn FnMut() -> Result<(), ImportError>,
) -> Result<Normalized, ImportError> {
    checkpoint()?;
    let mut data = GltfLoadData {
        meshopt_views: decode_meshopt_views(&document, raw, &mut buffers, checkpoint)?,
        ..GltfLoadData::default()
    };
    data.instances = read_instances(&document, raw, &buffers, checkpoint)?;
    let plan = quantized_accessors(document.as_json())?;
    let bakes = uv_bakes(document.as_json(), raw);
    if plan.is_empty() && bakes.is_empty() {
        return Ok(Normalized {
            document,
            buffers,
            data,
        });
    }
    data.quantized_accessors = plan.len();
    data.baked_uv_materials = bakes.keys().map(|(material, _)| *material).collect();
    let mut json = document.into_json();
    if !plan.is_empty() {
        rewrite_as_floats(&mut json, &mut buffers, &plan, checkpoint)?;
    }
    if !bakes.is_empty() {
        bake_uv_transforms(&mut json, &mut buffers, &bakes, checkpoint)?;
    }
    let document = gltf::Document::from_json(json).map_err(|e| ImportError::Gltf(e.to_string()))?;
    Ok(Normalized {
        document,
        buffers,
        data,
    })
}

// ---------------------------------------------------------------------------------------------
// EXT_meshopt_compression
// ---------------------------------------------------------------------------------------------

fn meshopt_error(view: usize, why: impl std::fmt::Display) -> ImportError {
    extension_error(
        GLTF_MESHOPT_INVALID,
        MESHOPT,
        format!("buffer view {view}: {why}"),
    )
}

/// Decodes every compressed view into its buffer; returns how many there were.
///
/// A decoded view lands at the view's own offset, so the accessors stay as they are. A buffer whose
/// data is not in the file is grown to fit; the growth is limited by what was decoded, or a small file
/// declaring a view at a huge offset would ask for that much memory.
fn decode_meshopt_views(
    document: &gltf::Document,
    raw: &Value,
    buffers: &mut [Vec<u8>],
    checkpoint: &mut dyn FnMut() -> Result<(), ImportError>,
) -> Result<usize, ImportError> {
    let views = array(raw, "bufferViews");
    let mut blocks = Vec::new();
    for (i, view) in views.iter().enumerate() {
        if let Some(block) = view.pointer("/extensions/EXT_meshopt_compression") {
            blocks.push((i, MeshoptView::from_json(block).map_err(|e| meshopt_error(i, e))?));
        }
    }
    if blocks.is_empty() {
        return Ok(0);
    }
    let json = document.as_json();
    let total: u64 = blocks
        .iter()
        .map(|(_, block)| block.decoded_len().unwrap_or(u64::MAX))
        .fold(0, u64::saturating_add);
    if total > MAX_TOTAL_DECODED_BYTES {
        return Err(extension_error(
            GLTF_MESHOPT_INVALID,
            MESHOPT,
            format!("the compressed views decode to {total} bytes, more than {MAX_TOTAL_DECODED_BYTES}"),
        ));
    }
    let growth_limit = total.saturating_mul(2).saturating_add(1 << 16);
    for (i, block) in &blocks {
        checkpoint()?;
        let i = *i;
        let view = json
            .buffer_views
            .get(i)
            .ok_or_else(|| meshopt_error(i, "the view does not exist"))?;
        let offset = view.byte_offset.map_or(0, |o| o.0);
        let length = view.byte_length.0;
        if block.decoded_len() != Some(length) {
            return Err(meshopt_error(
                i,
                format!(
                    "count {} times byteStride {} is not the view's byteLength {length}",
                    block.count, block.byte_stride
                ),
            ));
        }
        let encoded_end = block.byte_offset.checked_add(block.byte_length);
        let decoded = {
            let encoded = buffers
                .get(block.buffer)
                .and_then(|source| {
                    let start = usize::try_from(block.byte_offset).ok()?;
                    let end = usize::try_from(encoded_end?).ok()?;
                    source.get(start..end)
                })
                .ok_or_else(|| meshopt_error(i, "the compressed range is outside its buffer"))?;
            decode_stream(block.mode, block.filter, block.byte_stride, block.count, encoded)
                .map_err(|e| meshopt_error(i, e))?
        };
        let end = offset
            .checked_add(length)
            .ok_or_else(|| meshopt_error(i, "the view's range overflows"))?;
        let target = buffers
            .get_mut(view.buffer.value())
            .ok_or_else(|| meshopt_error(i, "the view's buffer does not exist"))?;
        if (target.len() as u64) < end {
            if end > growth_limit {
                return Err(meshopt_error(
                    i,
                    format!("the view ends at byte {end}, far past the {total} bytes decoded"),
                ));
            }
            let end = usize::try_from(end).map_err(|_| meshopt_error(i, "the view is too large"))?;
            target.resize(end, 0);
        }
        let (start, end) = (offset as usize, end as usize);
        target
            .get_mut(start..end)
            .ok_or_else(|| meshopt_error(i, "the view is outside its buffer"))?
            .copy_from_slice(&decoded);
    }
    Ok(blocks.len())
}

// ---------------------------------------------------------------------------------------------
// Accessors as floats
// ---------------------------------------------------------------------------------------------

/// One component as a float. A normalized integer maps as glTF 2.0 § 3.11 says: an unsigned one by
/// `c / max`, a signed one by `max(c / max, -1)`.
fn component_to_f32(kind: ComponentType, normalized: bool, bytes: &[u8]) -> f32 {
    match (kind, bytes) {
        (ComponentType::I8, [a, ..]) => {
            let v = f32::from(*a as i8);
            if normalized { (v / 127.0).max(-1.0) } else { v }
        }
        (ComponentType::U8, [a, ..]) => {
            let v = f32::from(*a);
            if normalized { v / 255.0 } else { v }
        }
        (ComponentType::I16, [a, b, ..]) => {
            let v = f32::from(i16::from_le_bytes([*a, *b]));
            if normalized { (v / 32767.0).max(-1.0) } else { v }
        }
        (ComponentType::U16, [a, b, ..]) => {
            let v = f32::from(u16::from_le_bytes([*a, *b]));
            if normalized { v / 65535.0 } else { v }
        }
        (ComponentType::U32, [a, b, c, d, ..]) => u32::from_le_bytes([*a, *b, *c, *d]) as f32,
        (ComponentType::F32, [a, b, c, d, ..]) => f32::from_le_bytes([*a, *b, *c, *d]),
        _ => 0.0,
    }
}

/// `component` and `kind` of an accessor the validation accepted.
fn accessor_layout(accessor: &gltf::json::Accessor) -> Option<(ComponentType, gltf::json::accessor::Type)> {
    match (&accessor.component_type, &accessor.type_) {
        (Checked::Valid(GenericComponentType(component)), Checked::Valid(kind)) => Some((*component, *kind)),
        _ => None,
    }
}

/// The bytes of `count` elements at a view, checked against the buffer that is really there.
fn elements<'a>(
    json: &Root,
    buffers: &'a [Vec<u8>],
    view: usize,
    extra_offset: u64,
    count: u64,
    element: u64,
) -> Result<(&'a [u8], u64), String> {
    let view = json
        .buffer_views
        .get(view)
        .ok_or_else(|| format!("buffer view {view} does not exist"))?;
    let stride = view.byte_stride.map_or(element, |s| s.0 as u64).max(element);
    let start = view
        .byte_offset
        .map_or(0, |o| o.0)
        .checked_add(extra_offset)
        .ok_or("the range overflows")?;
    let end = stride
        .checked_mul(count.saturating_sub(1))
        .and_then(|n| n.checked_add(element))
        .and_then(|n| n.checked_add(start))
        .ok_or("the range overflows")?;
    let slice = buffers
        .get(view.buffer.value())
        .and_then(|bytes| bytes.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?))
        .ok_or("the data is outside its buffer")?;
    Ok((slice, stride))
}

/// Every component of accessor `index` as floats, sparse edits applied, in element order.
fn read_floats(json: &Root, buffers: &[Vec<u8>], index: usize) -> Result<Vec<f32>, String> {
    let accessor = json
        .accessors
        .get(index)
        .ok_or_else(|| format!("accessor {index} does not exist"))?;
    let (component, kind) = accessor_layout(accessor).ok_or("it has an invalid type")?;
    let components = kind.multiplicity();
    let size = component.size();
    let element = (size * components) as u64;
    let count = accessor.count.0;
    let mut out = match accessor.buffer_view {
        Some(view) => {
            let offset = accessor.byte_offset.map_or(0, |o| o.0);
            let (bytes, stride) = elements(json, buffers, view.value(), offset, count, element)?;
            let mut out = Vec::with_capacity(count as usize * components);
            for i in 0..count as usize {
                let at = i * stride as usize;
                for c in 0..components {
                    out.push(component_to_f32(
                        component,
                        accessor.normalized,
                        bytes.get(at + c * size..).unwrap_or_default(),
                    ));
                }
            }
            out
        }
        None if count <= MAX_VIEWLESS_ELEMENTS => vec![0.0; count as usize * components],
        None => return Err(format!("{count} elements with no buffer view is too many")),
    };
    if let Some(sparse) = &accessor.sparse {
        let index_kind = match &sparse.indices.component_type {
            Checked::Valid(c) => c.0,
            Checked::Invalid => return Err("its sparse indices have an invalid type".into()),
        };
        let edits = sparse.count.0;
        let (indices, index_stride) = elements(
            json,
            buffers,
            sparse.indices.buffer_view.value(),
            sparse.indices.byte_offset.0,
            edits,
            index_kind.size() as u64,
        )?;
        let (values, value_stride) = elements(
            json,
            buffers,
            sparse.values.buffer_view.value(),
            sparse.values.byte_offset.0,
            edits,
            element,
        )?;
        for edit in 0..edits as usize {
            let at = edit * index_stride as usize;
            let target = component_to_f32(index_kind, false, indices.get(at..).unwrap_or_default()) as usize;
            let slot = out
                .get_mut(
                    target.saturating_mul(components)..target.saturating_add(1).saturating_mul(components),
                )
                .ok_or_else(|| format!("a sparse index reaches element {target} of {count}"))?;
            for (c, value) in slot.iter_mut().enumerate() {
                *value = component_to_f32(
                    component,
                    accessor.normalized,
                    values
                        .get(edit * value_stride as usize + c * size..)
                        .unwrap_or_default(),
                );
            }
        }
    }
    Ok(out)
}

/// How an attribute accessor is used, for its rewrite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Use {
    /// A `POSITION`: it needs `min` and `max`.
    position: bool,
    /// A color: floats below 0 mean nothing.
    color: bool,
}

/// The accessors that must become floats, and the error for a component type no reader takes.
fn quantized_accessors(json: &Root) -> Result<BTreeMap<usize, Use>, ImportError> {
    use gltf::Semantic as S;
    let refuse = |what: &str, accessor: usize, kind: ComponentType| {
        extension_error(
            GLTF_QUANTIZATION_INVALID,
            QUANTIZATION,
            format!("{what} accessor {accessor} cannot use component type {kind:?}"),
        )
    };
    let mut plan: BTreeMap<usize, Use> = BTreeMap::new();
    for mesh in &json.meshes {
        for primitive in &mesh.primitives {
            for (semantic, index) in &primitive.attributes {
                let Checked::Valid(semantic) = semantic else {
                    continue;
                };
                let i = index.value();
                let Some(accessor) = json.accessors.get(i) else {
                    continue; // the validation reports it
                };
                let Some((component, _)) = accessor_layout(accessor) else {
                    continue;
                };
                let integer = component != ComponentType::F32;
                match semantic {
                    S::Positions | S::Normals | S::Tangents => {
                        if component == ComponentType::U32 {
                            return Err(refuse("a vertex", i, component));
                        }
                        if integer {
                            let entry = plan.entry(i).or_default();
                            entry.position |= matches!(semantic, S::Positions);
                        }
                    }
                    S::TexCoords(_) => match component {
                        ComponentType::U32 => return Err(refuse("a TEXCOORD", i, component)),
                        ComponentType::I8 | ComponentType::I16 => {
                            plan.entry(i).or_default();
                        }
                        // The reader always normalizes these, so a plain integer goes through here.
                        ComponentType::U8 | ComponentType::U16 if !accessor.normalized => {
                            plan.entry(i).or_default();
                        }
                        _ => {}
                    },
                    // The reader normalizes unsigned colors by their range whatever the flag says.
                    // That is the right reading: glTF has no color above 1 in an integer type, and
                    // leaving `normalized` out is a writer's mistake. A signed color never gets here
                    // through the checked parse (the extension has no COLOR_n row, ADR-5958); it stays
                    // decodable for a document normalized without that check.
                    S::Colors(_) => match component {
                        ComponentType::U32 => return Err(refuse("a COLOR", i, component)),
                        ComponentType::I8 | ComponentType::I16 => {
                            plan.entry(i).or_default().color = true;
                        }
                        _ => {}
                    },
                    S::Joints(_) if !matches!(component, ComponentType::U8 | ComponentType::U16) => {
                        return Err(refuse("a JOINTS", i, component));
                    }
                    S::Weights(_)
                        if !matches!(
                            component,
                            ComponentType::U8 | ComponentType::U16 | ComponentType::F32
                        ) =>
                    {
                        return Err(refuse("a WEIGHTS", i, component));
                    }
                    // Valid joints and weights, and application attributes.
                    _ => {}
                }
            }
            for target in primitive.targets.iter().flatten() {
                for index in [target.positions, target.normals, target.tangents]
                    .into_iter()
                    .flatten()
                {
                    let i = index.value();
                    if let Some((component, _)) = json.accessors.get(i).and_then(accessor_layout) {
                        if component == ComponentType::U32 {
                            return Err(refuse("a morph target", i, component));
                        }
                        if component != ComponentType::F32 {
                            plan.entry(i).or_default();
                        }
                    }
                }
            }
        }
    }
    Ok(plan)
}

/// Replaces the planned accessors with float accessors in one new buffer.
fn rewrite_as_floats(
    json: &mut Root,
    buffers: &mut Vec<Vec<u8>>,
    plan: &BTreeMap<usize, Use>,
    checkpoint: &mut dyn FnMut() -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let invalid = |accessor: usize, why: String| {
        extension_error(
            GLTF_QUANTIZATION_INVALID,
            QUANTIZATION,
            format!("accessor {accessor}: {why}"),
        )
    };
    let mut floats: Vec<u8> = Vec::new();
    let buffer_index =
        u32::try_from(buffers.len()).map_err(|_| ImportError::Gltf("too many buffers".to_string()))?;
    let mut planned = 0u64;
    for &index in plan.keys() {
        let elements = json.accessors.get(index).and_then(|accessor| {
            let (_, kind) = accessor_layout(accessor)?;
            accessor.count.0.checked_mul(kind.multiplicity() as u64 * 4)
        });
        planned = planned.saturating_add(elements.unwrap_or(u64::MAX));
        if planned > MAX_NORMALIZED_BYTES {
            return Err(invalid(
                index,
                format!("the integer attributes become more than {MAX_NORMALIZED_BYTES} bytes of floats"),
            ));
        }
    }
    for (&index, usage) in plan {
        checkpoint()?;
        let mut values = read_floats(json, buffers, index).map_err(|e| invalid(index, e))?;
        if usage.color {
            for value in &mut values {
                *value = value.max(0.0);
            }
        }
        let offset = floats.len() as u64;
        floats.reserve(values.len() * 4);
        for value in &values {
            floats.extend_from_slice(&value.to_le_bytes());
        }
        let view_index = u32::try_from(json.buffer_views.len())
            .map_err(|_| ImportError::Gltf("too many buffer views".to_string()))?;
        json.buffer_views.push(gltf::json::buffer::View {
            buffer: gltf::json::Index::new(buffer_index),
            byte_length: USize64(values.len() as u64 * 4),
            byte_offset: Some(USize64(offset)),
            byte_stride: None,
            name: None,
            target: None,
            extensions: None,
            extras: Default::default(),
        });
        let accessor = json
            .accessors
            .get_mut(index)
            .ok_or_else(|| invalid(index, "it does not exist".to_string()))?;
        accessor.buffer_view = Some(gltf::json::Index::new(view_index));
        accessor.byte_offset = None;
        accessor.component_type = Checked::Valid(GenericComponentType(ComponentType::F32));
        accessor.normalized = false;
        accessor.sparse = None;
        if usage.position {
            // `min` and `max` are in the stored integers (glTF 2.0 § 3.6.2.5); the new accessor needs
            // them in its floats.
            let mut min = [f32::INFINITY; 3];
            let mut max = [f32::NEG_INFINITY; 3];
            for point in values.as_chunks::<3>().0 {
                for axis in 0..3 {
                    min[axis] = min[axis].min(point[axis]);
                    max[axis] = max[axis].max(point[axis]);
                }
            }
            accessor.min = Some(serde_json::json!(min));
            accessor.max = Some(serde_json::json!(max));
        }
    }
    json.buffers.push(gltf::json::Buffer {
        byte_length: USize64(floats.len() as u64),
        name: None,
        uri: None,
        extensions: None,
        extras: Default::default(),
    });
    buffers.push(floats);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// KHR_texture_transform over quantized UVs
// ---------------------------------------------------------------------------------------------

/// The core texture slots of a raw material, in the order that decides between disagreeing transforms.
fn material_texture_infos(material: &Value) -> [Option<&Value>; 5] {
    [
        material.pointer("/pbrMetallicRoughness/baseColorTexture"),
        material.pointer("/pbrMetallicRoughness/metallicRoughnessTexture"),
        material.get("normalTexture"),
        material.get("occlusionTexture"),
        material.get("emissiveTexture"),
    ]
}

/// The UV set a texture reads and its transform, if any. A `texCoord` in the block wins.
fn texture_uv_set(info: &Value) -> (u32, Option<TextureTransform>) {
    let transform = info
        .pointer("/extensions/KHR_texture_transform")
        .map(TextureTransform::from_block);
    let set = transform
        .and_then(|t| t.tex_coord)
        .or_else(|| {
            info.get("texCoord")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        })
        .unwrap_or(0);
    (set, transform)
}

/// Which `(material, UV set)` must have its `KHR_texture_transform` baked into the vertices.
///
/// A quantizer stores a UV set as integers over its own range and uses the texture transform to scale
/// it back, so those values mean nothing until the transform applies, and the renderer has no UV
/// transform. A set is baked when a primitive of the material holds it as integers; every primitive of
/// that material then gets it, floats included, since they share the one transform. Float UVs of any
/// other material keep the transform in the side table, as before.
fn uv_bakes(json: &Root, raw: &Value) -> BTreeMap<(usize, u32), TextureTransform> {
    use gltf::Semantic;
    let mut out = BTreeMap::new();
    let materials = array(raw, "materials");
    for primitive in json.meshes.iter().flat_map(|mesh| &mesh.primitives) {
        let Some(material) = primitive.material.map(|m| m.value()) else {
            continue;
        };
        for (semantic, index) in &primitive.attributes {
            let Checked::Valid(Semantic::TexCoords(set)) = semantic else {
                continue;
            };
            let integer = json
                .accessors
                .get(index.value())
                .and_then(accessor_layout)
                .is_some_and(|(component, _)| component != ComponentType::F32);
            if !integer || out.contains_key(&(material, *set)) {
                continue;
            }
            let Some(raw_material) = materials.get(material) else {
                continue;
            };
            if let Some(transform) = material_set_transform(raw_material, material, *set) {
                out.insert((material, *set), transform);
            }
        }
    }
    out
}

/// The transform a material gives UV set `set`: its first texture's, with a warning when another
/// texture on the set wants something else, since one set of vertices can only hold one result.
fn material_set_transform(material: &Value, index: usize, set: u32) -> Option<TextureTransform> {
    let on_set: Vec<Option<TextureTransform>> = material_texture_infos(material)
        .into_iter()
        .flatten()
        .map(texture_uv_set)
        .filter(|(uv_set, _)| *uv_set == set)
        .map(|(_, transform)| transform)
        .collect();
    let first = on_set.iter().flatten().next().copied()?;
    let same = |other: &Option<TextureTransform>| {
        let other = other.unwrap_or(TextureTransform::IDENTITY);
        (other.offset, other.rotation, other.scale) == (first.offset, first.rotation, first.scale)
    };
    if !on_set.iter().all(same) {
        tracing::warn!(
            target: "regia_asset_pipeline",
            material = index,
            uv_set = set,
            "KHR_texture_transform: the material's textures disagree on a quantized UV set; baking the first"
        );
    }
    Some(first)
}

/// Gives each baked UV set a new float accessor, one per accessor and transform, and points the
/// primitives at it. The source accessor stays for anything else that reads it.
fn bake_uv_transforms(
    json: &mut Root,
    buffers: &mut Vec<Vec<u8>>,
    bakes: &BTreeMap<(usize, u32), TextureTransform>,
    checkpoint: &mut dyn FnMut() -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    use gltf::Semantic;
    let invalid = |accessor: usize, why: String| {
        extension_error(
            GLTF_QUANTIZATION_INVALID,
            QUANTIZATION,
            format!("accessor {accessor}: {why}"),
        )
    };
    let buffer_index =
        u32::try_from(buffers.len()).map_err(|_| ImportError::Gltf("too many buffers".to_string()))?;
    let mut baked: Vec<u8> = Vec::new();
    // Key: the source accessor and the transform's bits, so primitives that share both share the result.
    let mut made: BTreeMap<(usize, [u32; 5]), u32> = BTreeMap::new();
    let mut meshes = std::mem::take(&mut json.meshes);
    let mut result = Ok(());
    'meshes: for mesh in &mut meshes {
        for primitive in &mut mesh.primitives {
            if let Err(error) = checkpoint() {
                result = Err(error);
                break 'meshes;
            }
            let Some(material) = primitive.material.map(|m| m.value()) else {
                continue;
            };
            for (semantic, index) in &mut primitive.attributes {
                let Checked::Valid(Semantic::TexCoords(set)) = semantic else {
                    continue;
                };
                let Some(transform) = bakes.get(&(material, *set)) else {
                    continue;
                };
                let source = index.value();
                let bits = [
                    transform.offset[0].to_bits(),
                    transform.offset[1].to_bits(),
                    transform.rotation.to_bits(),
                    transform.scale[0].to_bits(),
                    transform.scale[1].to_bits(),
                ];
                let new_index = match made.get(&(source, bits)) {
                    Some(existing) => *existing,
                    None => match bake_one(json, buffers, &mut baked, buffer_index, source, transform) {
                        Ok(new_index) => {
                            made.insert((source, bits), new_index);
                            new_index
                        }
                        Err(why) => {
                            result = Err(invalid(source, why));
                            break 'meshes;
                        }
                    },
                };
                *index = gltf::json::Index::new(new_index);
            }
        }
    }
    json.meshes = meshes;
    result?;
    json.buffers.push(gltf::json::Buffer {
        byte_length: USize64(baked.len() as u64),
        name: None,
        uri: None,
        extensions: None,
        extras: Default::default(),
    });
    buffers.push(baked);
    Ok(())
}

/// One new accessor: `source` read as floats, transformed. Returns its index.
fn bake_one(
    json: &mut Root,
    buffers: &[Vec<u8>],
    baked: &mut Vec<u8>,
    buffer_index: u32,
    source: usize,
    transform: &TextureTransform,
) -> Result<u32, String> {
    let accessor = json
        .accessors
        .get(source)
        .ok_or_else(|| "it does not exist".to_string())?;
    let (_, kind) = accessor_layout(accessor).ok_or("it has an invalid type")?;
    if kind != gltf::json::accessor::Type::Vec2 {
        return Err(format!("a TEXCOORD must be a Vec2, not {kind:?}"));
    }
    let size = accessor.count.0.saturating_mul(8);
    if size.saturating_add(baked.len() as u64) > MAX_NORMALIZED_BYTES {
        return Err(format!(
            "the baked UVs become more than {MAX_NORMALIZED_BYTES} bytes of floats"
        ));
    }
    let values = read_floats(json, buffers, source)?;
    let offset = baked.len() as u64;
    baked.reserve(values.len() * 4);
    for uv in values.as_chunks::<2>().0 {
        for value in transform.apply(*uv) {
            baked.extend_from_slice(&value.to_le_bytes());
        }
    }
    let view_index = u32::try_from(json.buffer_views.len()).map_err(|_| "too many buffer views")?;
    let accessor_index = u32::try_from(json.accessors.len()).map_err(|_| "too many accessors")?;
    json.buffer_views.push(gltf::json::buffer::View {
        buffer: gltf::json::Index::new(buffer_index),
        byte_length: USize64(baked.len() as u64 - offset),
        byte_offset: Some(USize64(offset)),
        byte_stride: None,
        name: None,
        target: None,
        extensions: None,
        extras: Default::default(),
    });
    json.accessors.push(gltf::json::Accessor {
        buffer_view: Some(gltf::json::Index::new(view_index)),
        byte_offset: None,
        count: USize64(values.len() as u64 / 2),
        component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
        extensions: None,
        extras: Default::default(),
        type_: Checked::Valid(gltf::json::accessor::Type::Vec2),
        min: None,
        max: None,
        name: None,
        normalized: false,
        sparse: None,
    });
    Ok(accessor_index)
}

// ---------------------------------------------------------------------------------------------
// EXT_mesh_gpu_instancing
// ---------------------------------------------------------------------------------------------

/// Reads each node's instance transforms. A node with no mesh, a skinned node or an empty block draws
/// no instances (with a warning): the first has nothing to instance, and the extension does not define
/// the second.
fn read_instances(
    document: &gltf::Document,
    raw: &Value,
    buffers: &[Vec<u8>],
    checkpoint: &mut dyn FnMut() -> Result<(), ImportError>,
) -> Result<BTreeMap<usize, Vec<[f32; 16]>>, ImportError> {
    let mut out = BTreeMap::new();
    let mut total = 0usize;
    let json = document.as_json();
    for (i, node) in array(raw, "nodes").iter().enumerate() {
        checkpoint()?;
        let Some(block) = node.pointer("/extensions/EXT_mesh_gpu_instancing") else {
            continue;
        };
        let skip = |why: &str| {
            tracing::warn!(target: "regia_asset_pipeline", node = i, "{INSTANCING}: {why}; no instances");
        };
        if node.get("mesh").is_none() {
            skip("the node has no mesh");
            continue;
        }
        if node.get("skin").is_some() {
            skip("the node is skinned");
            continue;
        }
        let invalid =
            |why: String| extension_error(GLTF_INSTANCING_INVALID, INSTANCING, format!("node {i}: {why}"));
        let attribute =
            |key: &str, want: gltf::json::accessor::Type| -> Result<Option<Vec<f32>>, ImportError> {
                let Some(value) = block.get("attributes").and_then(|a| a.get(key)) else {
                    return Ok(None);
                };
                let index = value
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| invalid(format!("{key} is not an accessor index")))?;
                let kind = json
                    .accessors
                    .get(index)
                    .and_then(accessor_layout)
                    .map(|(_, k)| k);
                if kind != Some(want) {
                    return Err(invalid(format!("{key} accessor {index} is not a {want:?}")));
                }
                read_floats(json, buffers, index)
                    .map(Some)
                    .map_err(|e| invalid(format!("{key} accessor {index}: {e}")))
            };
        use gltf::json::accessor::Type;
        let translations = attribute("TRANSLATION", Type::Vec3)?;
        let rotations = attribute("ROTATION", Type::Vec4)?;
        let scales = attribute("SCALE", Type::Vec3)?;
        let counts = [
            translations.as_ref().map(|v| v.len() / 3),
            rotations.as_ref().map(|v| v.len() / 4),
            scales.as_ref().map(|v| v.len() / 3),
        ];
        let mut known = counts.into_iter().flatten();
        let Some(count) = known.next() else {
            skip("the block has no TRANSLATION, ROTATION or SCALE");
            continue;
        };
        if known.any(|other| other != count) {
            return Err(invalid(format!(
                "the attributes disagree on the instance count: {counts:?}"
            )));
        }
        total = total.saturating_add(count);
        if total > MAX_FILE_INSTANCES {
            return Err(invalid(format!(
                "the file has more than {MAX_FILE_INSTANCES} instances"
            )));
        }
        let mut matrices = Vec::with_capacity(count);
        for n in 0..count {
            if n.is_multiple_of(4096) {
                checkpoint()?;
            }
            let vec3 = |values: &Option<Vec<f32>>, default: Vec3| {
                values
                    .as_ref()
                    .map_or(default, |v| Vec3::new(v[n * 3], v[n * 3 + 1], v[n * 3 + 2]))
            };
            let translation = vec3(&translations, Vec3::ZERO);
            let scale = vec3(&scales, Vec3::ONE);
            let rotation = rotations.as_ref().map_or(Quat::IDENTITY, |v| {
                Quat::from_xyzw(v[n * 4], v[n * 4 + 1], v[n * 4 + 2], v[n * 4 + 3])
            });
            if !translation.is_finite() || !scale.is_finite() || !rotation.is_finite() {
                return Err(invalid(format!("instance {n} is not finite")));
            }
            let rotation = if rotation.length_squared() > 1e-12 {
                rotation.normalize()
            } else {
                return Err(invalid(format!("instance {n} has a zero rotation")));
            };
            matrices
                .push(Mat4::from_scale_rotation_translation(scale, rotation, translation).to_cols_array());
        }
        out.insert(i, matrices);
    }
    Ok(out)
}

/// One extension a file names, as `asset.gltf_extensions` reports it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GltfExtensionUse {
    /// The extension's name.
    pub name: String,
    /// Listed in `extensionsRequired`.
    pub required: bool,
    /// What the importer does with it; `None` when it cannot read it (a required one never reaches a
    /// report, an optional one falls back).
    pub support: Option<ExtensionSupport>,
}

/// What a glTF's extensions amount to, without cooking anything (`asset.gltf_extensions`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct GltfExtensionReport {
    /// Every extension the file names, sorted.
    pub extensions: Vec<GltfExtensionUse>,
    /// The optional extensions the importer falls back from, sorted.
    pub fallbacks: Vec<String>,
    /// Compressed buffer views that decode.
    pub meshopt_views: usize,
    /// Integer attribute accessors read as floats.
    pub quantized_accessors: usize,
    /// Materials whose `KHR_texture_transform` was baked into quantized UVs, sorted.
    pub baked_uv_materials: Vec<usize>,
    /// `(node index, instance count)` of every instanced node.
    pub instanced_nodes: Vec<(usize, usize)>,
    /// `KHR_materials_variants` names.
    pub variants: Vec<String>,
    /// Primitives with a `KHR_materials_variants` mapping.
    pub variant_primitives: usize,
    /// `KHR_lights_punctual` lights readable in the file.
    pub lights: usize,
    /// Nodes that carry one of those lights.
    pub light_nodes: usize,
}

impl GltfExtensionReport {
    /// Builds a report from the file's extension table and what opening it found.
    pub(crate) fn new(table: &crate::gltf_extensions::GltfExtensionTable, data: &GltfLoadData) -> Self {
        let mut names: Vec<&String> = table.used.iter().chain(&table.required).collect();
        names.sort();
        names.dedup();
        Self {
            extensions: names
                .into_iter()
                .map(|name| GltfExtensionUse {
                    name: name.clone(),
                    required: table.required.contains(name),
                    support: extension_support(name),
                })
                .collect(),
            fallbacks: table.fallbacks(),
            meshopt_views: data.meshopt_views,
            quantized_accessors: data.quantized_accessors,
            baked_uv_materials: data.baked_uv_materials.iter().copied().collect(),
            instanced_nodes: data.instances.iter().map(|(n, list)| (*n, list.len())).collect(),
            variants: table.variant_names.clone(),
            variant_primitives: table.variants.len(),
            lights: table.lights.len(),
            light_nodes: table.node_lights.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_map_as_the_specification_says() {
        let snorm8 = |v: i8| component_to_f32(ComponentType::I8, true, &[v as u8]);
        assert_eq!(snorm8(127), 1.0);
        assert_eq!(snorm8(-127), -1.0);
        assert_eq!(snorm8(-128), -1.0, "the extra negative value clamps");
        let snorm16 = |v: i16| component_to_f32(ComponentType::I16, true, &v.to_le_bytes());
        assert_eq!(snorm16(32767), 1.0);
        assert_eq!(snorm16(-32768), -1.0);
        assert_eq!(component_to_f32(ComponentType::U8, true, &[255]), 1.0);
        assert_eq!(component_to_f32(ComponentType::U8, false, &[255]), 255.0);
        assert_eq!(component_to_f32(ComponentType::I8, false, &[0xff]), -1.0);
        assert_eq!(component_to_f32(ComponentType::U16, true, &[255, 255]), 1.0);
        assert_eq!(component_to_f32(ComponentType::U16, false, &[1, 1]), 257.0);
        assert_eq!(
            component_to_f32(ComponentType::F32, false, &[]),
            0.0,
            "short input"
        );
    }

    #[test]
    fn support_is_known_for_what_the_importer_reads_only() {
        assert_eq!(
            extension_support("KHR_mesh_quantization"),
            Some(ExtensionSupport::Cooked)
        );
        assert_eq!(
            extension_support("KHR_texture_transform"),
            Some(ExtensionSupport::SideTable)
        );
        assert_eq!(
            extension_support("KHR_lights_punctual"),
            Some(ExtensionSupport::SideTable)
        );
        for name in [
            "KHR_draco_mesh_compression",
            "KHR_texture_basisu",
            "VENDOR_anything",
            "",
        ] {
            assert_eq!(extension_support(name), None, "{name}");
        }
    }
}
