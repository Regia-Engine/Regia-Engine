// SPDX-License-Identifier: MPL-2.0
//! Final-artifact audit receipts (ADR-8348).
//!
//! A staging audit proves what was staged. A receipt proves what the final, generated package
//! contained when this tool inspected it: it binds the artifact's BLAKE3, the effective policy
//! digest, the container settings and the tool version to the coverage of that inspection.
//! Receipts are sidecar files; the artifact itself (possibly signed) is never modified.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{AuditError, Policy, Report};

/// Receipt schema identifier.
pub const RECEIPT_SCHEMA: &str = "regia.package_hygiene.receipt";
/// Receipt schema version. A different version never verifies.
pub const RECEIPT_VERSION: u32 = 1;
/// Container kinds this tool opens. Anything else is opaque bytes or listed as uninspected.
pub const SUPPORTED_CONTAINERS: &[&str] = &["directory", "fpak", "gzip", "tar", "tar.gz", "zip"];
/// Suffix appended to the artifact file name for its receipt.
pub const RECEIPT_SUFFIX: &str = ".hygiene-receipt.json";

/// Caller hints for a final-artifact audit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactOptions {
    /// In-archive prefixes (such as `Game/` or `assets/`) that are stripped to find the
    /// staging-relative path an allowance names. The full in-archive path also matches.
    pub allow_prefixes: Vec<String>,
    /// Directory for transient spool files: a nested container larger than the free memory
    /// budget is streamed once (and scanned) into a generated file here, opened, then removed.
    /// Default: the artifact's parent directory. If it cannot be written the entry fails closed
    /// with `nested_limit`.
    pub spool_dir: Option<PathBuf>,
}

/// One container that was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerCoverage {
    /// Opaque scan-local identity (`location-N`, with `::<asset-id>` for pak entries).
    pub location_id: String,
    /// Privacy-scrubbed display path.
    pub path: String,
    /// `directory`, `zip`, `tar`, `tar.gz`, `gzip` or `fpak`.
    pub kind: String,
    /// Nesting level; the artifact is 0.
    pub depth: u32,
    /// Entries listed by the container.
    pub entries: u64,
}

/// An entry or container whose payload this tool did not decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UninspectedEntry {
    /// Opaque scan-local identity.
    pub location_id: String,
    /// Privacy-scrubbed display path.
    pub path: String,
    /// `zip_entry` or a container format (`zstd`, `xz`, `bzip2`, `7z`, `rar`, `squashfs`,
    /// `lz4`, `appimage`, `cab`, `xar`, `ar`).
    pub kind: String,
    /// `encrypted`, `compression_method` or `unsupported_container`.
    pub reason: String,
}

/// What the final-artifact audit covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// [`SUPPORTED_CONTAINERS`] of this tool version.
    pub supported: Vec<String>,
    /// True when the scan finished and nothing is uninspected.
    pub complete: bool,
    /// Containers opened, in traversal order.
    pub containers: Vec<ContainerCoverage>,
    /// Entries and containers not decoded, in traversal order.
    pub uninspected: Vec<UninspectedEntry>,
    /// ZIP, tar and gzip entries visited (pak entries are `hygiene.pak_entries_scanned`).
    pub archive_entries: u64,
    /// Deepest container level opened.
    pub max_depth: u32,
}

/// Versioned, deterministic evidence of one final-artifact audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Receipt {
    /// [`RECEIPT_SCHEMA`].
    pub schema: String,
    /// [`RECEIPT_VERSION`].
    pub version: u32,
    /// `regia_cli <version>` that produced the receipt.
    pub tool: String,
    /// Privacy-scrubbed artifact file name (`.` for a directory). Never an absolute path.
    pub artifact: String,
    /// Container kind of the artifact itself (`zip`, `tar.gz`, `directory`, `fpak`, `file`, or
    /// an unsupported format name).
    pub artifact_kind: String,
    /// BLAKE3 of the artifact bytes (`b3:` plus hex); for a directory, of its canonical listing.
    pub artifact_hash: String,
    /// Artifact size; for a directory, the sum of its file sizes.
    pub artifact_bytes: u64,
    /// The effective policy digest, as in `hygiene.policy_digest`.
    pub policy_digest: String,
    /// `strict_containers` in force.
    pub strict_containers: bool,
    /// `max_container_depth` in force.
    pub max_container_depth: u32,
    /// What was opened and what was not.
    pub coverage: Coverage,
    /// The scan report: counters, limits and (for a rejected artifact) findings.
    pub hygiene: Report,
}

impl Receipt {
    /// Deterministic pretty JSON with a trailing newline. Equal inputs give equal bytes.
    ///
    /// # Errors
    /// Serialization failure (not expected for this type).
    pub fn to_json(&self) -> Result<String, AuditError> {
        let mut text = serde_json::to_string_pretty(self)
            .map_err(|_| AuditError::new("package_hygiene_io", "receipt"))?;
        text.push('\n');
        Ok(text)
    }
}

/// The fields a verifier binds. Unknown fields are ignored so a receipt reader stays small.
#[derive(Deserialize)]
struct Binding {
    schema: String,
    version: u32,
    tool: String,
    artifact_hash: String,
    artifact_bytes: u64,
    policy_digest: String,
    strict_containers: bool,
    max_container_depth: u32,
    coverage: BindingCoverage,
    hygiene: BindingReport,
}

#[derive(Deserialize)]
struct BindingCoverage {
    complete: bool,
}

#[derive(Deserialize)]
struct BindingReport {
    complete: bool,
    findings: Vec<serde_json::Value>,
}

/// `regia_cli <version>`, the tool string receipts carry.
#[must_use]
pub fn tool_version() -> String {
    format!("regia_cli {}", env!("CARGO_PKG_VERSION"))
}

/// Audit a final artifact (a ZIP/APK/AAB, tar, tar.gz, `.fpak`, other file, or a directory)
/// in place. The receipt's report may contain findings; see [`check_artifact`].
///
/// Nothing is extracted or written. Entry names are validated but never used as paths.
///
/// # Errors
/// Invalid policy, unreadable or changed artifact, malformed or ambiguous archive
/// (`package_hygiene_archive`), unsafe entry name (`package_hygiene_path`), or a limit.
pub fn audit_artifact(
    artifact: &Path,
    policy: &Policy,
    options: &ArtifactOptions,
) -> Result<Receipt, AuditError> {
    let (artifact_hash, artifact_bytes) = hash_artifact(artifact)?;
    let outcome = super::scan::artifact::run(artifact, policy, options)?;
    // A changed artifact must not get a receipt for the bytes hashed before the scan.
    if hash_artifact(artifact)? != (artifact_hash.clone(), artifact_bytes) {
        return Err(AuditError::new("package_hygiene_io", "artifact"));
    }
    let state = outcome.state;
    Ok(Receipt {
        schema: RECEIPT_SCHEMA.to_owned(),
        version: RECEIPT_VERSION,
        tool: tool_version(),
        artifact: outcome.display,
        artifact_kind: outcome.kind.to_owned(),
        artifact_hash,
        artifact_bytes,
        policy_digest: outcome.report.policy_digest.clone(),
        strict_containers: policy.strict_containers,
        max_container_depth: policy.max_container_depth,
        coverage: Coverage {
            supported: SUPPORTED_CONTAINERS
                .iter()
                .map(|kind| (*kind).to_owned())
                .collect(),
            complete: outcome.report.complete && state.uninspected.is_empty(),
            containers: state.containers,
            uninspected: state.uninspected,
            archive_entries: state.archive_entries,
            max_depth: state.deepest,
        },
        hygiene: outcome.report,
    })
}

/// Audit a final artifact and reject findings; with `strict_containers`, also reject any
/// uninspected entry (`package_hygiene_uninspected`).
///
/// # Errors
/// Everything [`audit_artifact`] returns, `package_hygiene_failed` with the report, or
/// `package_hygiene_uninspected` naming the first uninspected location.
pub fn check_artifact(
    artifact: &Path,
    policy: &Policy,
    options: &ArtifactOptions,
) -> Result<Receipt, AuditError> {
    let receipt = audit_artifact(artifact, policy, options)?;
    let report = &receipt.hygiene;
    if !report.findings.is_empty() {
        return Err(AuditError {
            code: "package_hygiene_failed",
            location: "final artifact rejected".into(),
            location_id: None,
            matcher_bytes: report.matcher_bytes,
            matchers_compiled: report.matchers_compiled,
            report: Some(Box::new(receipt.hygiene)),
        });
    }
    if policy.strict_containers
        && let Some(first) = receipt.coverage.uninspected.first()
    {
        return Err(AuditError {
            code: "package_hygiene_uninspected",
            location: first.path.clone(),
            location_id: Some(first.location_id.clone()),
            matcher_bytes: report.matcher_bytes,
            matchers_compiled: report.matchers_compiled,
            report: Some(Box::new(receipt.hygiene)),
        });
    }
    Ok(receipt)
}

/// Check that a receipt still describes `artifact` under `policy` and this tool: schema,
/// version, tool, artifact hash and size, policy digest and container settings, and that it
/// records a complete, clean scan. The artifact is hashed, not rescanned.
///
/// # Errors
/// `package_hygiene_receipt` with the first mismatching field as location (`receipt` for
/// unreadable JSON, `hygiene` for a receipt of a rejected or incomplete scan), or the errors
/// of reading the artifact and compiling the policy.
pub fn verify_receipt(artifact: &Path, receipt_json: &str, policy: &Policy) -> Result<(), AuditError> {
    let mismatch = |field: &str| AuditError::new("package_hygiene_receipt", field);
    let binding: Binding = serde_json::from_str(receipt_json).map_err(|_| mismatch("receipt"))?;
    if binding.schema != RECEIPT_SCHEMA {
        return Err(mismatch("schema"));
    }
    if binding.version != RECEIPT_VERSION {
        return Err(mismatch("version"));
    }
    if binding.tool != tool_version() {
        return Err(mismatch("tool"));
    }
    if !binding.hygiene.complete || !binding.hygiene.findings.is_empty() {
        return Err(mismatch("hygiene"));
    }
    if binding.strict_containers != policy.strict_containers {
        return Err(mismatch("strict_containers"));
    }
    if binding.strict_containers && !binding.coverage.complete {
        return Err(mismatch("coverage"));
    }
    if binding.max_container_depth != policy.max_container_depth {
        return Err(mismatch("max_container_depth"));
    }
    if binding.policy_digest != super::policy::Compiled::new(policy)?.digest {
        return Err(mismatch("policy_digest"));
    }
    let (hash, bytes) = hash_artifact(artifact)?;
    if bytes != binding.artifact_bytes {
        return Err(mismatch("artifact_bytes"));
    }
    if hash != binding.artifact_hash {
        return Err(mismatch("artifact_hash"));
    }
    Ok(())
}

/// The receipt sidecar of an artifact: `<artifact>.hygiene-receipt.json` beside it.
#[must_use]
pub fn receipt_path(artifact: &Path) -> PathBuf {
    let mut name = artifact
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(RECEIPT_SUFFIX);
    artifact.with_file_name(name)
}

/// Write a receipt beside its artifact atomically (temporary file, then rename) and answer
/// its path. The artifact is not touched.
///
/// # Errors
/// `package_hygiene_io` at `receipt` when the sidecar cannot be written.
pub fn write_receipt(artifact: &Path, receipt: &Receipt) -> Result<PathBuf, AuditError> {
    let path = receipt_path(artifact);
    let text = receipt.to_json()?;
    let mut tmp_name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let io = |_| AuditError::new("package_hygiene_io", "receipt");
    std::fs::write(&tmp, text).map_err(io)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(err));
    }
    Ok(path)
}

/// Remove a stale receipt so an old sidecar never sits beside a new artifact.
///
/// # Errors
/// `package_hygiene_io` at `receipt` when an existing sidecar cannot be removed.
pub fn remove_receipt(artifact: &Path) -> Result<(), AuditError> {
    match std::fs::remove_file(receipt_path(artifact)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(AuditError::new("package_hygiene_io", "receipt")),
    }
}

/// BLAKE3 of a file, or of a directory's canonical listing (sorted relative path, size and
/// file hash per regular file). Symlinks and special files are refused.
fn hash_artifact(artifact: &Path) -> Result<(String, u64), AuditError> {
    let io = || AuditError::new("package_hygiene_io", "artifact");
    let meta = std::fs::symlink_metadata(artifact).map_err(|_| io())?;
    if meta.is_file() {
        let (hash, bytes) = hash_file(artifact).ok_or_else(io)?;
        return Ok((format!("b3:{}", hash.to_hex()), bytes));
    }
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(AuditError::new("package_hygiene_path", "artifact"));
    }
    let mut files = Vec::new();
    let mut stack = vec![artifact.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|_| io())? {
            let path = entry.map_err(|_| io())?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|_| io())?;
            if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
                return Err(AuditError::new("package_hygiene_path", "artifact"));
            }
            if meta.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(artifact)
                    .ok()
                    .and_then(Path::to_str)
                    .ok_or_else(|| AuditError::new("package_hygiene_path", "artifact"))?
                    .replace('\\', "/");
                files.push((rel, path));
            }
        }
    }
    files.sort();
    let mut tree = blake3::Hasher::new();
    tree.update(b"regia.package_hygiene.tree.v1\0");
    let mut total = 0u64;
    for (rel, path) in files {
        let (hash, bytes) = hash_file(&path).ok_or_else(io)?;
        total = total.saturating_add(bytes);
        tree.update(rel.as_bytes());
        tree.update(&[0]);
        tree.update(&bytes.to_le_bytes());
        tree.update(hash.as_bytes());
    }
    Ok((format!("b3:{}", tree.finalize().to_hex()), total))
}

fn hash_file(path: &Path) -> Option<(blake3::Hash, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Some((hasher.finalize(), total))
}
