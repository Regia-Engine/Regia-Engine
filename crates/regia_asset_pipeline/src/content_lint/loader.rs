// SPDX-License-Identifier: MPL-2.0
//! Admission before typed parsing, buffer IO and extension normalization.
use std::path::Path;

use serde_json::Value;

use super::io::{StableFile, metadata};
use super::{ContentBudget, ContentError, product};
use crate::gltf_import::{checked_parse, decode_gltf_uri, reject_hostile_uris};
use crate::gltf_normalize::{is_fallback_buffer, normalize_with_checkpoint};

fn invalid(error: impl std::fmt::Display) -> ContentError {
    ContentError::Invalid(error.to_string())
}

fn array<'a>(raw: &'a Value, key: &str) -> &'a [Value] {
    raw.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}
fn count(accessor: &Value) -> Result<u64, ContentError> {
    accessor
        .get("count")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("accessor count must be an unsigned integer"))
}
fn components(accessor: &Value) -> Result<u64, ContentError> {
    match accessor.get("type").and_then(Value::as_str) {
        Some("SCALAR") => Ok(1),
        Some("VEC2") => Ok(2),
        Some("VEC3") => Ok(3),
        Some("VEC4" | "MAT2") => Ok(4),
        Some("MAT3") => Ok(9),
        Some("MAT4") => Ok(16),
        _ => Err(invalid("invalid accessor type")),
    }
}
fn accessor_reservation(accessor: &Value, budget: &mut ContentBudget<'_>) -> Result<(), ContentError> {
    let elements = product(count(accessor)?, components(accessor)?)?;
    // Source float expansion, temporary float vector, destination Vec growth slack.
    budget.decoded(product(elements, 16)?)?;
    budget.work(elements)?;
    Ok(())
}

pub(crate) fn preflight(raw: &Value, budget: &mut ContentBudget<'_>) -> Result<(), ContentError> {
    let accessors = array(raw, "accessors");
    for accessor in accessors {
        accessor_reservation(accessor, budget)?;
    }
    // A UV accessor may be baked once per distinct primitive transform; reserve
    // every use conservatively, even when the normalizer can share the output.
    for mesh in array(raw, "meshes") {
        for primitive in array(mesh, "primitives") {
            if let Some(attributes) = primitive.get("attributes").and_then(Value::as_object) {
                for (semantic, index) in attributes {
                    if semantic.starts_with("TEXCOORD_")
                        && let Some(accessor) = index
                            .as_u64()
                            .and_then(|i| usize::try_from(i).ok())
                            .and_then(|i| accessors.get(i))
                    {
                        accessor_reservation(accessor, budget)?;
                    }
                }
            }
        }
    }
    for view in array(raw, "bufferViews") {
        if view.pointer("/extensions/EXT_meshopt_compression").is_some() {
            let length = view
                .get("byteLength")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("invalid view length"))?;
            let offset = view.get("byteOffset").and_then(Value::as_u64).unwrap_or(0);
            let end = offset
                .checked_add(length)
                .ok_or(ContentError::Limit("decoded view range"))?;
            // Decoder output + destination resize (including Vec capacity slack).
            budget.decoded(product(end, 3)?)?;
            budget.work(length)?;
        }
    }
    for node in array(raw, "nodes") {
        if let Some(attributes) = node
            .pointer("/extensions/EXT_mesh_gpu_instancing/attributes")
            .and_then(Value::as_object)
        {
            let mut instances = 0_u64;
            for index in attributes.values() {
                if let Some(accessor) = index
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| accessors.get(i))
                {
                    accessor_reservation(accessor, budget)?;
                    instances = instances.max(count(accessor)?);
                }
            }
            budget.decoded(product(instances, 64)?)?;
            budget.work(instances)?;
        }
    }
    Ok(())
}

pub(crate) fn open(
    path: &Path,
    budget: &mut ContentBudget<'_>,
) -> Result<(gltf::Document, Vec<gltf::buffer::Data>), ContentError> {
    let mut input = metadata(path, budget)?;
    budget.checkpoint()?;
    super::json::admit(&input.json, budget)?;
    let raw: Value = serde_json::from_slice(&input.json).map_err(invalid)?;
    preflight(&raw, budget)?;
    drop(raw);
    budget.checkpoint()?;
    let (gltf, raw) = checked_parse(&input.json).map_err(invalid)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    reject_hostile_uris(&gltf.document, Some(base)).map_err(invalid)?;
    // Open and size every external buffer before any buffer allocation. Keep the
    // descriptors and verify both named path and descriptor stamps after IO.
    let mut external = Vec::new();
    for buffer in gltf.document.buffers() {
        budget.work(1)?;
        let file = match buffer.source() {
            gltf::buffer::Source::Uri(uri)
                if !uri.starts_with("data:") && !is_fallback_buffer(&raw, buffer.index()) =>
            {
                let decoded = decode_gltf_uri(uri).ok_or_else(|| invalid("URI is not UTF-8"))?;
                let path = crate::cook::join_portable(base, &decoded).map_err(invalid)?;
                let file = StableFile::open(&path, budget)?;
                budget.external(file.len())?;
                budget.decoded(super::padded(file.len())?)?;
                Some(file)
            }
            _ => None,
        };
        external.push(file);
    }
    let mut buffers = Vec::new();
    for buffer in gltf.document.buffers() {
        budget.checkpoint()?;
        if is_fallback_buffer(&raw, buffer.index()) {
            buffers.push(Vec::new());
            continue;
        }
        let bytes = match buffer.source() {
            gltf::buffer::Source::Bin => {
                let (offset, length) = input.bin.ok_or_else(|| invalid("buffer has no GLB BIN chunk"))?;
                budget.decoded(length)?;
                input.source.read(offset, length, budget, true)?
            }
            gltf::buffer::Source::Uri(uri) if uri.starts_with("data:") => decode_uri(uri, budget)?,
            gltf::buffer::Source::Uri(_) => {
                let file = external
                    .get_mut(buffer.index())
                    .and_then(Option::as_mut)
                    .ok_or_else(|| invalid("missing external buffer"))?;
                file.read(0, file.len(), budget, true)?
            }
        };
        if bytes.len() < buffer.length() {
            return Err(invalid(format!(
                "buffer {} shorter than declared byteLength",
                buffer.index()
            )));
        }
        buffers.push(bytes);
    }
    budget.checkpoint()?;
    let mut cancellation = None;
    let normalized = normalize_with_checkpoint(gltf.document, &raw, buffers, &mut || {
        budget.checkpoint().map_err(|error| {
            let message = error.to_string();
            cancellation = Some(error);
            crate::ImportError::Gltf(message)
        })
    });
    if let Some(error) = cancellation {
        return Err(error);
    }
    let normalized = normalized.map_err(invalid)?;
    budget.checkpoint()?;
    input.source.verify()?;
    for file in external.iter().flatten() {
        file.verify()?;
    }
    Ok((
        normalized.document,
        normalized.buffers.into_iter().map(gltf::buffer::Data).collect(),
    ))
}

/// The same bounded stages over owned fuzz input; external IO is deliberately
/// unavailable. URI validation still runs, so arbitrary URI strings are covered.
pub(crate) fn open_bytes(bytes: &[u8], budget: &mut ContentBudget<'_>) -> Result<(), ContentError> {
    let mut cursor = std::io::Cursor::new(bytes);
    let scanned = super::io::scan(&mut cursor, bytes.len() as u64, budget)?;
    super::json::admit(&scanned.json, budget)?;
    let raw: Value = serde_json::from_slice(&scanned.json).map_err(invalid)?;
    preflight(&raw, budget)?;
    drop(raw);
    let (gltf, raw) = checked_parse(&scanned.json).map_err(invalid)?;
    // In-memory fuzz inputs must never trigger even filesystem metadata lookups.
    // External payloads are unavailable here, so lexical relative-path validation
    // is sufficient; the file loader above keeps the full project sandbox.
    crate::gltf_import::reject_uris_with(&gltf.document, |uri| {
        regia_core::project_path::check_relative(uri).map_err(|error| error.to_string())
    })
    .map_err(invalid)?;
    let mut buffers = Vec::new();
    for buffer in gltf.document.buffers() {
        budget.work(1)?;
        if is_fallback_buffer(&raw, buffer.index()) {
            buffers.push(Vec::new());
            continue;
        }
        let data = match buffer.source() {
            gltf::buffer::Source::Bin => {
                let (offset, length) = scanned.bin.ok_or_else(|| invalid("missing BIN"))?;
                budget.decoded(length)?;
                super::io::read_range(&mut cursor, offset, length, budget, true)?
            }
            gltf::buffer::Source::Uri(uri) if uri.starts_with("data:") => decode_uri(uri, budget)?,
            gltf::buffer::Source::Uri(_) => return Err(invalid("external IO unavailable in byte harness")),
        };
        if data.len() < buffer.length() {
            return Err(invalid("short buffer"));
        }
        buffers.push(data);
    }
    let mut cancellation = None;
    let normalized = normalize_with_checkpoint(gltf.document, &raw, buffers, &mut || {
        budget.checkpoint().map_err(|error| {
            let message = error.to_string();
            cancellation = Some(error);
            crate::ImportError::Gltf(message)
        })
    });
    if let Some(error) = cancellation {
        return Err(error);
    }
    normalized.map_err(invalid)?;
    budget.checkpoint()
}

fn decode_uri(uri: &str, budget: &mut ContentBudget<'_>) -> Result<Vec<u8>, ContentError> {
    use base64::Engine;
    let (header, encoded) = uri.split_once(',').ok_or_else(|| invalid("malformed data URI"))?;
    if !header.ends_with(";base64") {
        return Err(invalid("data URI is not base64-encoded"));
    }
    let encoded = encoded.trim();
    let length = super::padded(product((encoded.len() as u64).div_ceil(4), 3)?)?;
    budget.decoded(length)?;
    budget.work(encoded.len() as u64)?;
    budget.allocated(length)?;
    let length = usize::try_from(length).map_err(|_| ContentError::Limit("address space"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| ContentError::Limit("allocation"))?;
    bytes.resize(length, 0);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode_slice(encoded, &mut bytes)
        .map_err(invalid)?;
    bytes.truncate(super::padded(decoded as u64)? as usize);
    Ok(bytes)
}
