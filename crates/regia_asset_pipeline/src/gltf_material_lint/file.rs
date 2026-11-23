// SPDX-License-Identifier: MPL-2.0
//! Bounded JSON input, streamed GLB preservation and atomic metadata replacement.
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    MaterialLintError, MaterialLintReport, MaterialLintSettings, invalid, lint_materials_with_checkpoint,
};
use crate::content_lint::io::{BLOCK, MetadataInput, metadata};
use crate::content_lint::{ContentBudget, ContentError};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn io_error(e: impl std::fmt::Display) -> MaterialLintError {
    MaterialLintError {
        code: "gltf_material_io",
        message: e.to_string(),
    }
}
fn input_error(error: ContentError) -> MaterialLintError {
    let code = match &error {
        ContentError::Invalid(_) => "gltf_material_invalid",
        ContentError::Io(_) => "gltf_material_io",
        _ => error.code(),
    };
    MaterialLintError {
        code,
        message: error.to_string(),
    }
}

/// Lint a `.gltf` or `.glb` source, optionally replacing metadata atomically.
/// Images and external buffers are never opened. GLB tail chunks are inspected
/// by their headers; fix copies their bytes in 64 KiB blocks. Effective limits
/// and deterministic counters are returned in the report. Serialize writers.
///
/// # Errors
/// Malformed metadata, IO failure, resource exhaustion or changed source.
pub fn lint_material_file(
    path: &Path,
    settings: &MaterialLintSettings,
    fix: bool,
) -> Result<MaterialLintReport, MaterialLintError> {
    let mut budget = ContentBudget::new(settings.input_limits.clone(), &|| false);
    lint_material_file_bounded(path, settings, fix, &mut budget)
}

/// File lint with cancellation and explicit shared limits. The operation
/// budget overrides `settings.input_limits`. Cancellation never publishes a fix.
///
/// # Errors
/// As `lint_material_file`, including `content_lint_cancelled`.
pub fn lint_material_file_bounded(
    path: &Path,
    settings: &MaterialLintSettings,
    fix: bool,
    budget: &mut ContentBudget<'_>,
) -> Result<MaterialLintReport, MaterialLintError> {
    let _span = tracing::info_span!("gltf_material_lint", fix, profile = settings.profile.name()).entered();
    budget.checkpoint().map_err(input_error)?;
    let kind = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if !kind.eq_ignore_ascii_case("gltf") && !kind.eq_ignore_ascii_case("glb") {
        return Err(invalid("expected a .gltf or .glb source"));
    }
    let original = fs::symlink_metadata(path).map_err(io_error)?;
    if fix && original.file_type().is_symlink() {
        return Err(io_error("fix refuses symlinks"));
    }
    let mut input = metadata(path, budget).map_err(input_error)?;
    if kind.eq_ignore_ascii_case("glb") && input.tail.is_none() {
        return Err(invalid("expected GLB header"));
    }
    if kind.eq_ignore_ascii_case("gltf") && input.tail.is_some() {
        return Err(invalid("expected JSON glTF for .gltf suffix"));
    }
    budget.checkpoint().map_err(input_error)?;
    crate::content_lint::json::admit(&input.json, budget).map_err(input_error)?;
    let mut document: serde_json::Value =
        serde_json::from_slice(&input.json).map_err(|e| invalid(e.to_string()))?;
    let materials = document
        .get("materials")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len) as u64;
    budget
        .work(
            crate::content_lint::product(materials, settings.name_rules.len() as u64 + 1)
                .map_err(input_error)?,
        )
        .map_err(input_error)?;
    let mut report = lint_materials_with_checkpoint(&mut document, settings, fix, &mut || {
        budget.checkpoint().map_err(input_error)
    })?;
    budget.checkpoint().map_err(input_error)?;
    input.source.verify().map_err(input_error)?;
    if report.changed {
        // A bounded writer refuses growth before serde appends beyond the JSON ceiling.
        let mut encoded = JsonWriter {
            bytes: Vec::new(),
            limit: budget.limits().max_json_bytes,
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer_pretty(&mut encoded, &document) {
            if encoded.exceeded {
                return Err(input_error(ContentError::Limit("max_json_bytes")));
            }
            return Err(invalid(error.to_string()));
        }
        let padding = if input.tail.is_some() {
            (4 - encoded.bytes.len() % 4) % 4
        } else {
            1
        };
        if (encoded.bytes.len() as u64).saturating_add(padding as u64) > encoded.limit {
            return Err(input_error(ContentError::Limit("max_json_bytes")));
        }
        if input.tail.is_some() {
            encoded.write_all(&[b' '; 3][..padding]).map_err(io_error)?;
        } else {
            encoded.write_all(b"\n").map_err(io_error)?;
        }
        replace(path, &mut input, &encoded.bytes, original.permissions(), budget).map_err(input_error)?;
    }
    report.input_limits = budget.limits().clone();
    report.input_usage = budget.usage().clone();
    Ok(report)
}

struct JsonWriter {
    bytes: Vec<u8>,
    limit: u64,
    exceeded: bool,
}
impl Write for JsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if (self.bytes.len() as u64).saturating_add(bytes.len() as u64) > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other("max_json_bytes"));
        }
        let required = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("JSON size overflow"))?;
        if required > self.bytes.capacity() {
            let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
            let target = self.bytes.capacity().saturating_mul(2).max(required).min(limit);
            self.bytes
                .try_reserve_exact(target - self.bytes.len())
                .map_err(std::io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn replace(
    path: &Path,
    input: &mut MetadataInput,
    json: &[u8],
    permissions: fs::Permissions,
    budget: &mut ContentBudget<'_>,
) -> Result<(), ContentError> {
    budget.checkpoint()?;
    input.source.verify()?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(format!(".material-lint-{}-{id}.tmp", std::process::id()));
    let temp = std::path::PathBuf::from(temp_name);
    let mut file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let written = (|| -> Result<(), ContentError> {
        budget.copied(json.len() as u64 + if input.tail.is_some() { 20 } else { 0 })?;
        if let Some(tail) = input.tail {
            let total = 20_u64
                .checked_add(json.len() as u64)
                .and_then(|n| n.checked_add(input.source.len() - tail))
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(ContentError::Limit("GLB size"))?;
            file.write_all(b"glTF")?;
            file.write_all(&2_u32.to_le_bytes())?;
            file.write_all(&total.to_le_bytes())?;
            file.write_all(&(json.len() as u32).to_le_bytes())?;
            file.write_all(b"JSON")?;
        }
        file.write_all(json)?;
        if let Some(tail) = input.tail {
            input.source.file.seek(SeekFrom::Start(tail))?;
            let mut remaining = input.source.len() - tail;
            let mut block = [0; BLOCK];
            while remaining != 0 {
                let n = remaining.min(BLOCK as u64) as usize;
                budget.copied(n as u64)?;
                input.source.file.read_exact(&mut block[..n])?;
                file.write_all(&block[..n])?;
                remaining -= n as u64;
            }
        }
        budget.checkpoint()?;
        input.source.verify()?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    // If a concurrent truncate caused an IO failure, report the changed source.
    let written = input.source.verify().and(written);
    let result = written.and_then(|()| {
        budget.checkpoint()?;
        input.source.verify()?;
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(ContentError::Changed(path.display().to_string()));
        }
        fs::rename(&temp, path)?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
