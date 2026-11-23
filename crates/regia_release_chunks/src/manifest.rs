// SPDX-License-Identifier: MPL-2.0
//! `release-chunks` version 1: one JSON document, a TSV sibling and a SHA-256 sidecar.
//! Every field is always written so a later reader sees a stable shape.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::hash::{hex_encode, sha256_file};
use crate::{ChunkError, FORMAT_VERSION};

/// Registry name of this format (`docs/formats/release-chunks.md`).
pub const FORMAT_NAME: &str = "release-chunks";

/// Canonical project file name. `regia upgrade-project` owns this name and not a generic manifest.
pub const CANONICAL_FILE: &str = "release-chunks.json";

/// Serde field list for `cargo xtask check-formats`. Field order is the struct order below.
#[must_use]
pub fn schema_fingerprint() -> &'static str {
    "format_version,platform,tag,product_version,source_commit,libc_required,tier,chunks,links;name,group,buckets,asset,sha256,size,unpacked,files,content_key;path,target,chunk"
}

/// One shipped chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkRecord {
    /// Chunk name, unique inside the manifest.
    pub name: String,
    /// Group the files were packed under.
    pub group: String,
    /// Bucket count of the group. Cut writes 1. Pack never lowers a previous count.
    pub buckets: u32,
    /// Path of the chunk file relative to the manifest directory.
    pub asset: String,
    /// SHA-256 of the chunk file, lowercase hex.
    pub sha256: String,
    /// Chunk file size in bytes. This is the compressed size the cap applies to.
    pub size: u64,
    /// Unpacked payload bytes, with each hard-link set counted once.
    pub unpacked: u64,
    /// Archive member paths, sorted, `/` separators.
    pub files: Vec<String>,
    /// SHA-256 of the uncompressed tar. Pack reuses a previous chunk when this matches.
    pub content_key: String,
}

/// An installer link: `path` should be created as a link to `target` (the stored file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkLink {
    /// The link path, relative, `/` separators.
    pub path: String,
    /// The stored file the link points at.
    pub target: String,
    /// Chunk `name` that stores `target`.
    pub chunk: String,
}

/// A release-chunk manifest, format version [`FORMAT_VERSION`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkManifest {
    /// [`FORMAT_VERSION`] when written by this build.
    pub format_version: u32,
    /// Target platform label. Empty when the caller did not set one.
    pub platform: String,
    /// Release tag. Empty when the caller did not set one.
    pub tag: String,
    /// Product version string. Not a retail-baseline field.
    pub product_version: String,
    /// Source revision the tree was packed from. Empty when unknown.
    pub source_commit: String,
    /// Optional C library requirement the installer may read. Empty when unset.
    /// A consumer that still says `glibc_required` maps that key onto this field.
    pub libc_required: String,
    /// Tier label. Empty when the tree is not tiered. Any `[a-z0-9_-]+` value is a setting.
    pub tier: String,
    /// Chunks in group order, then name order.
    pub chunks: Vec<ChunkRecord>,
    /// Installer links. Empty when every path is stored in a chunk.
    pub links: Vec<ChunkLink>,
}

impl ChunkManifest {
    /// Refuses a newer `format_version`. Version 1 is stamped as [`FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// `release_chunks_newer` when `format_version` is above [`FORMAT_VERSION`]. The message
    /// names the version.
    pub fn migrate(mut self) -> Result<Self, ChunkError> {
        if self.format_version > FORMAT_VERSION {
            return Err(ChunkError::new(
                "release_chunks_newer",
                format!(
                    "release-chunks format_version {} is newer than this build (reads up to {FORMAT_VERSION})",
                    self.format_version
                ),
            ));
        }
        self.format_version = FORMAT_VERSION;
        Ok(self)
    }

    /// Canonical JSON: pretty, two-space indent, trailing newline, every field present.
    ///
    /// # Errors
    ///
    /// `manifest_invalid` when the value cannot be encoded.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, ChunkError> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| ChunkError::new("manifest_invalid", err.to_string()))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// TSV sibling. Columns are `format`, metadata, then `chunk`, `file` and `link` rows.
    #[must_use]
    pub fn to_tsv(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "format\t{FORMAT_VERSION}");
        let _ = writeln!(text, "platform\t{}", self.platform);
        let _ = writeln!(text, "tag\t{}", self.tag);
        let _ = writeln!(text, "product_version\t{}", self.product_version);
        let _ = writeln!(text, "source_commit\t{}", self.source_commit);
        let _ = writeln!(text, "libc_required\t{}", self.libc_required);
        let _ = writeln!(text, "tier\t{}", self.tier);
        for chunk in &self.chunks {
            let _ = writeln!(
                text,
                "chunk\t{}\t{}\t{}\t{}\t{}",
                chunk.name, chunk.asset, chunk.sha256, chunk.size, chunk.unpacked
            );
            for file in &chunk.files {
                let _ = writeln!(text, "file\t{}\t{file}", chunk.name);
            }
        }
        for link in &self.links {
            let _ = writeln!(text, "link\t{}\t{}", link.path, link.target);
        }
        text
    }
}

/// The example on the format page: one empty chunk, the well-known empty SHA-256.
#[must_use]
pub fn canonical_example() -> ChunkManifest {
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    ChunkManifest {
        format_version: FORMAT_VERSION,
        platform: "linux".to_string(),
        tag: "demo".to_string(),
        product_version: "0.6.0".to_string(),
        source_commit: "d6ed37677".to_string(),
        libc_required: String::new(),
        tier: String::new(),
        chunks: vec![ChunkRecord {
            name: "core-000".to_string(),
            group: "core".to_string(),
            buckets: 1,
            asset: "core-000.tar.zst".to_string(),
            sha256: empty.to_string(),
            size: 0,
            unpacked: 0,
            files: Vec::new(),
            content_key: empty.to_string(),
        }],
        links: Vec::new(),
    }
}

/// Reads a manifest and migrates it to [`FORMAT_VERSION`].
///
/// # Errors
///
/// `io_error`, `manifest_invalid` or `release_chunks_newer`.
pub fn read_manifest(path: &Path) -> Result<ChunkManifest, ChunkError> {
    let bytes = std::fs::read(path).map_err(|err| ChunkError::io(path, &err))?;
    parse_manifest(&bytes)
}

/// JSON bytes to a current manifest.
///
/// # Errors
///
/// `manifest_invalid` or `release_chunks_newer`.
pub fn parse_manifest(bytes: &[u8]) -> Result<ChunkManifest, ChunkError> {
    let manifest: ChunkManifest =
        serde_json::from_slice(bytes).map_err(|err| ChunkError::new("manifest_invalid", err.to_string()))?;
    manifest.migrate()
}

/// `format_version` of a JSON manifest, without migrating it.
///
/// # Errors
///
/// `manifest_invalid` when the bytes are not this format.
pub fn peek_version(bytes: &[u8]) -> Result<u32, ChunkError> {
    let manifest: ChunkManifest =
        serde_json::from_slice(bytes).map_err(|err| ChunkError::new("manifest_invalid", err.to_string()))?;
    Ok(manifest.format_version)
}

/// Parses, migrates and re-encodes. The corpus loader and `regia upgrade-project` use this.
///
/// # Errors
///
/// `manifest_invalid` or `release_chunks_newer`. A newer file's error names the version.
pub fn migrate_json(bytes: &[u8]) -> Result<Vec<u8>, ChunkError> {
    parse_manifest(bytes)?.to_json_bytes()
}

/// Writes `release-chunks.json`, `release-chunks.tsv` and `release-chunks.json.sha256`.
/// When `platform` is non-empty, also writes `manifest-<platform>[.<tier>].json` with the same bytes.
///
/// # Errors
///
/// `io_error` or `path_rejected` when the platform or tier is not a safe file token.
pub fn write_manifest_files(dir: &Path, manifest: &ChunkManifest) -> Result<(), ChunkError> {
    std::fs::create_dir_all(dir).map_err(|err| ChunkError::io(dir, &err))?;
    let json = manifest.to_json_bytes()?;
    let tsv = manifest.to_tsv();
    let json_path = dir.join(CANONICAL_FILE);
    let tsv_path = dir.join("release-chunks.tsv");
    write_atomic(&json_path, &json)?;
    write_atomic(&tsv_path, tsv.as_bytes())?;
    let json_sha = hex_encode(&sha256_file(&json_path)?);
    let tsv_sha = hex_encode(&sha256_file(&tsv_path)?);
    let sidecar = format!("{json_sha}  {CANONICAL_FILE}\n{tsv_sha}  release-chunks.tsv\n");
    write_atomic(&dir.join("release-chunks.json.sha256"), sidecar.as_bytes())?;
    if !manifest.platform.is_empty() {
        check_token("platform", &manifest.platform)?;
        let name = if manifest.tier.is_empty() {
            format!("manifest-{}.json", manifest.platform)
        } else {
            check_token("tier", &manifest.tier)?;
            format!("manifest-{}.{}.json", manifest.platform, manifest.tier)
        };
        write_atomic(&dir.join(name), &json)?;
    }
    Ok(())
}

fn check_token(field: &str, value: &str) -> Result<(), ChunkError> {
    let ok = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(ChunkError::new(
            "path_rejected",
            format!("{field} `{value}` must be [a-z0-9_-]+ when it is used in a file name"),
        ))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ChunkError> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.write_all(bytes).map_err(|err| ChunkError::io(&tmp, &err))?;
        file.flush().map_err(|err| ChunkError::io(&tmp, &err))?;
    }
    std::fs::rename(&tmp, path).map_err(|err| ChunkError::io(path, &err))?;
    Ok(())
}

/// Settings for a directory fingerprint. `magic`, when set, drops `.bin` files that do not start
/// with those bytes. Unset means every matching file is hashed.
#[derive(Debug, Clone)]
pub struct AuditSettings {
    /// Extensions without the dot. Default: `gltf`, `glb`, `bin`.
    pub extensions: Vec<String>,
    /// How many directory levels under the root to enter. Default: 3.
    pub depth: u32,
    /// Optional leading bytes a `.bin` file must have to be included.
    pub magic: Option<Vec<u8>>,
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            extensions: vec!["gltf".to_string(), "glb".to_string(), "bin".to_string()],
            depth: 3,
            magic: None,
        }
    }
}

/// One directory fingerprint: FNV-1a 64 of the sorted `(name, little-endian length)` records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPrint {
    /// Directory path relative to the audited root, `/` separators. Empty for the root.
    pub path: String,
    /// 16 lowercase hex digits.
    pub hash: String,
}
