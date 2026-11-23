// SPDX-License-Identifier: MPL-2.0
//! Product viewer package (track JW, item JW-L6, ADR-2540): what the web player and the Android
//! viewer load for one product, from static files.
//!
//! ```text
//! <package>/
//!   manifest.json     regia.viewer_package/1: ids, every file with its size and BLAKE3, package_hash
//!   mesh.rvm          every mesh (parts, part-swap alternatives, gem cuts per offered size), quantized
//!   recipe.json       the piece recipe (view_range, drives, gem slots), canonical JSON
//!   options.json      the option data: drives plus the option-graph product record without cost fields
//!   materials.json    the material entries the options can pick
//!   textures/...      optional, small PNG or JPEG (KTX2 passed through when a kit ships it)
//! ```
//!
//! The reader side ([`Package::from_files`]) needs no file system, threads or clock: the player
//! fetches the files however it likes and hands the bytes over; every size and hash is checked
//! before anything is used. The cook side (feature `cook`) builds packages from a part kit and a
//! recipe (`regia viewer pack`) and writes the stand-in kit (`regia viewer standin`).
//!
//! Page: `book/agents/viewer-pack.md`. Format: `docs/formats/viewer-package.md`.

pub mod geom;
pub mod mesh;

#[cfg(feature = "cook")]
pub mod cook;
#[cfg(feature = "cook")]
pub mod glb;
#[cfg(feature = "cook")]
pub mod standin;

use std::collections::BTreeMap;

use serde_json::{Value, json};

pub use mesh::MeshData;

/// The manifest's `schema` field.
pub const SCHEMA: &str = "regia.viewer_package/1";
/// The package format version this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The manifest file name.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The cooked mesh file name.
pub const MESH_FILE: &str = "mesh.rvm";
/// The recipe file name.
pub const RECIPE_FILE: &str = "recipe.json";
/// The option data file name.
pub const OPTIONS_FILE: &str = "options.json";
/// The material entries file name.
pub const MATERIALS_FILE: &str = "materials.json";
/// Largest manifest accepted.
pub const MAX_MANIFEST_BYTES: u64 = 1 << 20;
/// Largest single file accepted.
pub const MAX_FILE_BYTES: u64 = 64 << 20;
/// Largest package accepted.
pub const MAX_PACKAGE_BYTES: u64 = 256 << 20;
/// Most files in one package.
pub const MAX_FILES: usize = 512;
/// The size budget of a typical product (mesh + textures + data), bytes on the wire before HTTP
/// compression. The size report flags a package over it.
pub const BUDGET_BYTES: u64 = 3_000_000;

/// A refused package or cook input: a stable error code plus a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackError {
    /// Lowercase ascii code, part of the agent protocol (`book/agents/viewer-pack.md`).
    pub code: &'static str,
    /// What was wrong.
    pub message: String,
}

impl PackError {
    /// A new error.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        PackError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PackError {}

/// Every error code this crate returns, with its meaning (the agent page lists the same).
pub const ERROR_CODES: &[(&str, &str)] = &[
    ("pack_io", "a file could not be read or written"),
    (
        "pack_bad_manifest",
        "the manifest is not a regia.viewer_package/1 document",
    ),
    ("pack_version", "a format_version this build does not read"),
    (
        "pack_unsafe_path",
        "a file path leaves the package or uses characters outside a-z0-9._-/",
    ),
    ("pack_missing_file", "a file the manifest lists is not there"),
    ("pack_too_large", "a file or the package is over its cap"),
    (
        "pack_hash_mismatch",
        "a file's size or BLAKE3, or the package_hash, does not match",
    ),
    ("pack_bad_mesh", "mesh.rvm is damaged, or a mesh is malformed"),
    (
        "pack_bad_json",
        "recipe.json, options.json or materials.json is not JSON",
    ),
    (
        "pack_bad_kit",
        "the part kit is malformed (kit.json, a GLB, a node, a gem cut)",
    ),
    (
        "pack_bad_recipe",
        "the recipe is malformed or names a part or cut the kit lacks",
    ),
];

/// `blake3:<hex>` of some bytes.
pub fn blake3_hex(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Canonical JSON text: object keys sorted by code point at every level, no insignificant
/// whitespace, LF at the end. Independent of serde_json's map ordering feature.
pub fn canonical_json(v: &Value) -> String {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                let sorted: BTreeMap<&String, &Value> = m.iter().collect();
                out.push('{');
                for (i, (k, v)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    write(v, out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(v, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut s = String::new();
    write(v, &mut s);
    s.push('\n');
    s
}

/// Whether a package-relative path is safe: non-empty `/`-separated components of `a-z0-9._-`,
/// none of them `.` or `..`, at most 200 bytes.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 200
        && p.split('/').all(|c| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

/// Remove every `cost` and markup field from a JSON tree (interfaces.md § 12: dealer cost never
/// leaves the catalogue host). Returns how many fields were removed.
pub fn strip_cost(v: &mut Value) -> usize {
    const FIELDS: [&str; 5] = ["cost", "cost_cents", "markup", "markup_pct", "dealer_price"];
    match v {
        Value::Object(m) => {
            let mut n = 0;
            for f in FIELDS {
                if m.remove(f).is_some() {
                    n += 1;
                }
            }
            n + m.values_mut().map(strip_cost).sum::<usize>()
        }
        Value::Array(a) => a.iter_mut().map(strip_cost).sum(),
        _ => 0,
    }
}

/// One file of a package, as the manifest lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    /// Path inside the package.
    pub path: String,
    /// `mesh`, `recipe`, `options`, `materials` or `texture`.
    pub kind: String,
    /// Size in bytes.
    pub bytes: u64,
    /// `blake3:<hex>` of the content.
    pub blake3: String,
}

/// The `package_hash` of a file list: BLAKE3 over `path TAB bytes TAB blake3 LF` lines sorted by
/// path. A player can key its cache by it.
pub fn package_hash(files: &[FileEntry]) -> String {
    let mut sorted: Vec<&FileEntry> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut text = String::new();
    for f in sorted {
        text.push_str(&format!("{}\t{}\t{}\n", f.path, f.bytes, f.blake3));
    }
    blake3_hex(text.as_bytes())
}

/// The manifest of a package.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// The whole manifest document.
    pub doc: Value,
    /// Its file list.
    pub files: Vec<FileEntry>,
    /// Its `package_hash`.
    pub package_hash: String,
}

impl Manifest {
    /// Parse and check a manifest: schema, format version, file list (safe unique paths, sizes
    /// within the caps), and that `package_hash` matches the list.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, PackError> {
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(PackError::new("pack_too_large", "manifest.json is over 1 MiB"));
        }
        let bad = |m: &str| PackError::new("pack_bad_manifest", m.to_string());
        let doc: Value = serde_json::from_slice(bytes).map_err(|e| bad(&format!("not JSON: {e}")))?;
        if doc.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
            return Err(bad("schema is not regia.viewer_package/1"));
        }
        let v = doc
            .get("format_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| bad("no format_version"))?;
        if v != FORMAT_VERSION as u64 {
            return Err(PackError::new(
                "pack_version",
                format!(
                    "format_version {v}; this build reads {FORMAT_VERSION} (run `regia viewer pack` again)"
                ),
            ));
        }
        let files: Vec<FileEntry> = serde_json::from_value(doc.get("files").cloned().unwrap_or(Value::Null))
            .map_err(|e| bad(&format!("files: {e}")))?;
        if files.len() > MAX_FILES {
            return Err(PackError::new("pack_too_large", format!("{} files", files.len())));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0u64;
        for f in &files {
            if !safe_path(&f.path) || f.path == MANIFEST_FILE {
                return Err(PackError::new(
                    "pack_unsafe_path",
                    format!("file path {:?}", f.path),
                ));
            }
            if !seen.insert(f.path.as_str()) {
                return Err(bad(&format!("file {} listed twice", f.path)));
            }
            if f.bytes > MAX_FILE_BYTES {
                return Err(PackError::new(
                    "pack_too_large",
                    format!("{} is {} bytes", f.path, f.bytes),
                ));
            }
            total += f.bytes;
        }
        if total > MAX_PACKAGE_BYTES {
            return Err(PackError::new(
                "pack_too_large",
                format!("package is {total} bytes"),
            ));
        }
        for required in [MESH_FILE, RECIPE_FILE, OPTIONS_FILE, MATERIALS_FILE] {
            if !seen.contains(required) {
                return Err(bad(&format!("files does not list {required}")));
            }
        }
        let claimed = doc
            .get("package_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("no package_hash"))?;
        let actual = package_hash(&files);
        if claimed != actual {
            return Err(PackError::new(
                "pack_hash_mismatch",
                format!("package_hash {claimed}, files hash to {actual}"),
            ));
        }
        Ok(Manifest {
            doc,
            files,
            package_hash: actual,
        })
    }
}

/// A verified, decoded package.
#[derive(Clone, Debug)]
pub struct Package {
    /// The manifest.
    pub manifest: Manifest,
    /// Every mesh, by name.
    pub meshes: Vec<MeshData>,
    /// The recipe document.
    pub recipe: Value,
    /// The option data.
    pub options: Value,
    /// The material entries.
    pub materials: Value,
    /// The texture files (path, bytes), still encoded.
    pub textures: Vec<(String, Vec<u8>)>,
}

impl Package {
    /// Verify and decode a package from its files. `get` returns a file's bytes by path (`None`
    /// when missing). Every listed file's size and BLAKE3 is checked before it is parsed.
    pub fn from_files<'a>(
        manifest_bytes: &[u8],
        mut get: impl FnMut(&str) -> Option<&'a [u8]>,
    ) -> Result<Package, PackError> {
        let _span = tracing::info_span!("viewer_package_load", bytes = manifest_bytes.len()).entered();
        let manifest = Manifest::parse(manifest_bytes)?;
        let mut content: BTreeMap<String, &'a [u8]> = BTreeMap::new();
        for f in &manifest.files {
            let bytes = get(&f.path)
                .ok_or_else(|| PackError::new("pack_missing_file", format!("{} is missing", f.path)))?;
            if bytes.len() as u64 != f.bytes || blake3_hex(bytes) != f.blake3 {
                return Err(PackError::new(
                    "pack_hash_mismatch",
                    format!(
                        "{}: {} bytes, {}; the manifest says {} bytes, {}",
                        f.path,
                        bytes.len(),
                        blake3_hex(bytes),
                        f.bytes,
                        f.blake3
                    ),
                ));
            }
            content.insert(f.path.clone(), bytes);
        }
        let json = |name: &str| -> Result<Value, PackError> {
            let b = content.get(name).copied().unwrap_or_default();
            serde_json::from_slice(b).map_err(|e| PackError::new("pack_bad_json", format!("{name}: {e}")))
        };
        let meshes = mesh::decode(content.get(MESH_FILE).copied().unwrap_or_default())?;
        let recipe = json(RECIPE_FILE)?;
        let options = json(OPTIONS_FILE)?;
        let materials = json(MATERIALS_FILE)?;
        let textures = manifest
            .files
            .iter()
            .filter(|f| f.kind == "texture")
            .filter_map(|f| content.get(&f.path).map(|b| (f.path.clone(), b.to_vec())))
            .collect();
        Ok(Package {
            manifest,
            meshes,
            recipe,
            options,
            materials,
            textures,
        })
    }

    /// Find a mesh by name.
    pub fn mesh(&self, name: &str) -> Option<&MeshData> {
        self.meshes.iter().find(|m| m.name == name)
    }

    /// Triangles of every mesh.
    pub fn triangles(&self) -> usize {
        self.meshes.iter().map(MeshData::triangles).sum()
    }
}

/// Read and verify a package folder (desktop and Android; the web player uses
/// [`Package::from_files`] with fetched bytes).
#[cfg(not(target_arch = "wasm32"))]
pub fn read_dir(dir: &std::path::Path) -> Result<Package, PackError> {
    let root = match dir.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PackError::new(
                "pack_missing_file",
                format!("{} has no manifest.json", dir.display()),
            ));
        }
        Err(e) => {
            return Err(PackError::new("pack_io", format!("{}: {e}", dir.display())));
        }
    };
    let read = |name: &str, cap: u64| -> Result<Option<Vec<u8>>, PackError> {
        let p = dir.join(name);
        match std::fs::symlink_metadata(&p) {
            // Absent. Any other failure (permission denied, ...) is an IO error, not a missing file.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(PackError::new("pack_io", format!("{}: {e}", p.display()))),
            Ok(m) if !m.is_file() => Err(PackError::new(
                "pack_unsafe_path",
                format!("{name} is not a regular file"),
            )),
            Ok(m) if m.len() > cap => Err(PackError::new(
                "pack_too_large",
                format!("{name} is {} bytes", m.len()),
            )),
            Ok(_) => {
                // A directory symlink above this file is followed by the stat. Refuse one that
                // leaves the package. A final-component symlink is already not a regular file.
                let canon = p
                    .canonicalize()
                    .map_err(|e| PackError::new("pack_io", format!("{}: {e}", p.display())))?;
                if !canon.starts_with(&root) {
                    return Err(PackError::new(
                        "pack_unsafe_path",
                        format!("{name} links outside the package"),
                    ));
                }
                // Read at most `cap + 1` bytes: the file may have grown since the stat.
                use std::io::Read as _;
                let io = |e: std::io::Error| PackError::new("pack_io", format!("{}: {e}", p.display()));
                let mut bytes = Vec::new();
                std::fs::File::open(&canon)
                    .map_err(io)?
                    .take(cap.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(io)?;
                if bytes.len() as u64 > cap {
                    return Err(PackError::new(
                        "pack_too_large",
                        format!("{name} grew past {cap} bytes while it was read"),
                    ));
                }
                Ok(Some(bytes))
            }
        }
    };
    let manifest_bytes = read(MANIFEST_FILE, MAX_MANIFEST_BYTES)?.ok_or_else(|| {
        PackError::new(
            "pack_missing_file",
            format!("{} has no manifest.json", dir.display()),
        )
    })?;
    let manifest = Manifest::parse(&manifest_bytes)?;
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for f in &manifest.files {
        if let Some(b) = read(&f.path, MAX_FILE_BYTES)? {
            files.insert(f.path.clone(), b);
        }
    }
    Package::from_files(&manifest_bytes, |p| files.get(p).map(Vec::as_slice))
}

/// One row of a size report.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SizeRow {
    /// Path inside the package.
    pub path: String,
    /// File kind.
    pub kind: String,
    /// Bytes on disk.
    pub bytes: u64,
    /// Bytes after gzip level 9 (what a static host with compression sends), when measured.
    pub gzip_bytes: Option<u64>,
}

/// The size report of a package: per file, totals, and the budget check.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SizeReport {
    /// Every file including the manifest.
    pub files: Vec<SizeRow>,
    /// Sum of `bytes`.
    pub total_bytes: u64,
    /// Sum of `gzip_bytes` when every row has one.
    pub total_gzip_bytes: Option<u64>,
    /// `BUDGET_BYTES`.
    pub budget_bytes: u64,
    /// Whether `total_bytes` is within the budget.
    pub within_budget: bool,
    /// Triangles over every mesh.
    pub triangles: u64,
    /// Meshes in `mesh.rvm`.
    pub meshes: u64,
}

impl SizeReport {
    /// A report from rows (gzip sizes optional) and mesh counts.
    pub fn new(files: Vec<SizeRow>, meshes: u64, triangles: u64) -> Self {
        let total_bytes = files.iter().map(|f| f.bytes).sum();
        let total_gzip_bytes = files.iter().map(|f| f.gzip_bytes).sum::<Option<u64>>();
        SizeReport {
            files,
            total_bytes,
            total_gzip_bytes,
            budget_bytes: BUDGET_BYTES,
            within_budget: total_bytes <= BUDGET_BYTES,
            triangles,
            meshes,
        }
    }

    /// The report of a verified package (no gzip sizes; the cook adds them).
    pub fn of(package: &Package, manifest_bytes: u64) -> Self {
        let mut rows = vec![SizeRow {
            path: MANIFEST_FILE.into(),
            kind: "manifest".into(),
            bytes: manifest_bytes,
            gzip_bytes: None,
        }];
        rows.extend(package.manifest.files.iter().map(|f| SizeRow {
            path: f.path.clone(),
            kind: f.kind.clone(),
            bytes: f.bytes,
            gzip_bytes: None,
        }));
        SizeReport::new(rows, package.meshes.len() as u64, package.triangles() as u64)
    }

    /// As JSON.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

/// The format fingerprint `cargo xtask check-formats` hashes: the manifest's fields, the file
/// kinds and the `mesh.rvm` layout. Any change to one of them must bump `FORMAT_VERSION` (or
/// `mesh::MESH_FORMAT_VERSION`) and re-record the hash.
pub fn schema_fingerprint() -> String {
    format!(
        "{SCHEMA} v{FORMAT_VERSION}; manifest: schema format_version package_id house design_key product_keys \
         recipe_version recipe_hash kits[kit_id kit_version house] engine_version meshes[name material_slot vertices \
         triangles] files[path kind bytes blake3] package_hash; kinds: mesh recipe options materials texture; \
         required: {MESH_FILE} {RECIPE_FILE} {OPTIONS_FILE} {MATERIALS_FILE}; hash: blake3 of sorted path TAB bytes \
         TAB blake3 LF; rvm v{}: magic RVM1 u32 version u32 count u32 0; mesh: u16 name u16 slot pad4 u32 flags(1 uv, \
         2 idx32) u32 vertices u32 indices f32x3 min f32x3 max [f32x4 uv bounds] u16x3 positions pad4 i16x2 octahedral \
         normals [u16x2 uvs] u16|u32 indices pad4",
        mesh::MESH_FORMAT_VERSION
    )
}

/// Migration sources of the package format: none. A package is cooked data; a version change is
/// handled by cooking again (`regia viewer pack`), and the reader refuses other versions with
/// `pack_version`.
pub fn migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `regia upgrade-project`'s probe: the format version of a package's `manifest.json` or
/// `mesh.rvm`, `Ok(None)` for any other file (other JSON documents share the suffix).
pub fn probe_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if let Some(rest) = bytes.strip_prefix(mesh::MAGIC.as_slice()) {
        let v = rest.get(..4).ok_or("mesh.rvm: truncated header")?;
        return Ok(Some(u32::from_le_bytes([v[0], v[1], v[2], v[3]])));
    }
    let Ok(doc) = serde_json::from_slice::<Value>(bytes) else {
        return Ok(None);
    };
    if doc.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Ok(None);
    }
    let v = doc
        .get("format_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "manifest.json has no format_version".to_string())?;
    // `as u32` would read 2^32 + 1 as version 1.
    u32::try_from(v)
        .map(Some)
        .map_err(|_| format!("manifest.json format_version {v} is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit LOW (sprint-corr-web-viewer): `format_version` past `u32::MAX` was truncated by
    /// `as u32`, so `upgrade-project` read 4294967297 as version 1.
    #[test]
    fn corr_probe_never_truncates_a_version_past_u32() {
        let r = probe_version(br#"{"schema": "regia.viewer_package/1", "format_version": 4294967297}"#);
        assert!(r.is_err(), "{r:?}");
        let r = probe_version(br#"{"schema": "regia.viewer_package/1", "format_version": 4294967295}"#);
        assert_eq!(r, Ok(Some(u32::MAX)));
    }

    #[test]
    fn probe_reads_both_files_and_skips_others() {
        assert_eq!(probe_version(b"RVM1\x01\0\0\0rest"), Ok(Some(1)));
        assert!(probe_version(b"RVM1\x01").is_err());
        assert_eq!(
            probe_version(br#"{"schema": "regia.viewer_package/1", "format_version": 1}"#),
            Ok(Some(1))
        );
        assert_eq!(probe_version(br#"{"schema": "jewelry.build/1"}"#), Ok(None));
        assert_eq!(probe_version(b"not json"), Ok(None));
    }

    /// Seeded mutation of a real manifest: every mutant is refused with a code or parses; none
    /// panics (the registry's fuzz coverage of the JSON half; mesh.rs covers `mesh.rvm`).
    #[test]
    fn manifest_mutations_never_panic() {
        let files = vec![FileEntry {
            path: MESH_FILE.into(),
            kind: "mesh".into(),
            bytes: 4,
            blake3: blake3_hex(b"RVM1"),
        }];
        let mut all = files.clone();
        for f in [RECIPE_FILE, OPTIONS_FILE, MATERIALS_FILE] {
            all.push(FileEntry {
                path: f.into(),
                kind: "json".into(),
                bytes: 2,
                blake3: blake3_hex(b"{}"),
            });
        }
        let doc =
            json!({"schema": SCHEMA, "format_version": 1, "files": all, "package_hash": package_hash(&all)});
        let text = canonical_json(&doc).into_bytes();
        assert!(Manifest::parse(&text).is_ok());
        let mut s = 0x9E37_79B9u32;
        for _ in 0..4000 {
            let mut m = text.clone();
            for _ in 0..1 + (s % 4) {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let i = s as usize % m.len();
                m[i] = (s >> 8) as u8;
            }
            if let Err(e) = Manifest::parse(&m) {
                assert!(ERROR_CODES.iter().any(|(c, _)| *c == e.code), "{}", e.code);
            }
        }
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v = json!({"b": 1, "a": {"d": [1, {"z": 0, "y": 1}], "c": "x"}});
        assert_eq!(
            canonical_json(&v),
            "{\"a\":{\"c\":\"x\",\"d\":[1,{\"y\":1,\"z\":0}]},\"b\":1}\n"
        );
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for p in [
            "", "/abs", "../x", "a/../b", "a//b", "A.png", "a\\b", "c:x", "./a", "a/",
        ] {
            assert!(!safe_path(p), "{p:?}");
        }
        for p in ["mesh.rvm", "textures/detail_512.png", "a-b_c.d"] {
            assert!(safe_path(p), "{p:?}");
        }
    }

    #[test]
    fn strip_cost_reaches_every_level() {
        let mut v = json!({"cost": 1, "variants": [{"cost": 2, "price": 3, "addons": [{"markup": 1}]}]});
        assert_eq!(strip_cost(&mut v), 3);
        assert_eq!(v, json!({"variants": [{"price": 3, "addons": [{}]}]}));
    }

    /// A directory symlink must not let a manifest path read a file outside the package.
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn audit_read_dir_refuses_directory_symlink() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let tag = format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let dir = std::env::temp_dir().join(format!("regia-vp-sym-{tag}"));
        let outside = std::env::temp_dir().join(format!("regia-vp-sym-out-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::create_dir_all(&outside).expect("outside");
        let mesh = mesh::encode(&[MeshData {
            name: "part:x".into(),
            material_slot: "body".into(),
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: None,
            indices: vec![0, 1, 2],
        }])
        .expect("mesh");
        let recipe = canonical_json(&json!({})).into_bytes();
        let options = recipe.clone();
        let materials = recipe.clone();
        let tex = b"not-png".to_vec();
        std::fs::write(outside.join("a.png"), &tex).expect("tex");
        std::os::unix::fs::symlink(&outside, dir.join("textures")).expect("symlink");
        std::fs::write(dir.join(MESH_FILE), &mesh).expect("mesh");
        std::fs::write(dir.join(RECIPE_FILE), &recipe).expect("recipe");
        std::fs::write(dir.join(OPTIONS_FILE), &options).expect("options");
        std::fs::write(dir.join(MATERIALS_FILE), &materials).expect("materials");
        let files = vec![
            FileEntry {
                path: MESH_FILE.into(),
                kind: "mesh".into(),
                bytes: mesh.len() as u64,
                blake3: blake3_hex(&mesh),
            },
            FileEntry {
                path: RECIPE_FILE.into(),
                kind: "recipe".into(),
                bytes: recipe.len() as u64,
                blake3: blake3_hex(&recipe),
            },
            FileEntry {
                path: OPTIONS_FILE.into(),
                kind: "options".into(),
                bytes: options.len() as u64,
                blake3: blake3_hex(&options),
            },
            FileEntry {
                path: MATERIALS_FILE.into(),
                kind: "materials".into(),
                bytes: materials.len() as u64,
                blake3: blake3_hex(&materials),
            },
            FileEntry {
                path: "textures/a.png".into(),
                kind: "texture".into(),
                bytes: tex.len() as u64,
                blake3: blake3_hex(&tex),
            },
        ];
        let manifest = json!({
            "schema": SCHEMA,
            "format_version": FORMAT_VERSION,
            "files": files,
            "package_hash": package_hash(&files),
        });
        std::fs::write(dir.join(MANIFEST_FILE), canonical_json(&manifest)).expect("manifest");
        let err = read_dir(&dir).expect_err("a symlink out of the package is refused");
        assert_eq!(err.code, "pack_unsafe_path", "{err}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
