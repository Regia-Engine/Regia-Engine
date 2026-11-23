// SPDX-License-Identifier: MPL-2.0
//! The vetted layer set of one USD import, and the resolver the reader composes through.
//!
//! [`LayerSet::walk_file`] reads the root file and follows every sublayer, reference and payload
//! asset path each layer authors (in every variant, selected or not), depth first: each file is
//! read once, within the size caps, from inside the import root ([`super::import_root`]), and a
//! path back onto the current chain (a cycle) or a chain deeper than the limit is refused. Texture
//! files the layers name are read too, so the cook's input digest covers them. The composed stage
//! is then opened through [`VfsResolver`], which serves only those bytes: the reader never opens a
//! file of its own choosing.
//!
//! Identifiers are virtual absolute paths below the import root (`/assets/chair.usda`); a file
//! inside a package is `/assets/chair.usdz[textures/wood.png]`, the package-relative spelling of
//! the public asset-resolution documentation (openusd.org, "Asset Resolution (Ar)").

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openusd::ar::{self, AssetBuffer, ResolvedPath, SharedBuffer};
use openusd::{sdf, usd_utils, usdz};

use super::{UsdError, UsdLimits};

/// The bytes of every file the walk read, by identifier.
pub(super) type FileMap = BTreeMap<String, Arc<[u8]>>;

use super::is_usd_extension;

/// Image extensions a material may name (read into the set for the input digest).
fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "tga" | "bmp" | "exr" | "hdr" | "tif" | "tiff"
    )
}

/// The extension of an identifier's innermost path (the packaged file for `pkg.usdz[inner]`).
pub(super) fn extension_of(id: &str) -> String {
    let inner = if ar::is_package_relative_path(id) {
        ar::split_package_relative_path_inner(id).map_or_else(String::new, |(_, inner)| inner)
    } else {
        id.to_string()
    };
    let name = inner.rsplit('/').next().unwrap_or("");
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// The display form of an identifier: without the leading `/`.
pub(super) fn display(id: &str) -> String {
    id.strip_prefix('/').unwrap_or(id).to_string()
}

/// `path` (`/`-separated, relative) with `.` and `..` folded; `None` when a `..` leaves the top.
fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    Some(out.join("/"))
}

/// What an identifier is prefixed with when it can never resolve (absolute host paths, paths
/// leaving the import root or a package): the resolver finds no file under it.
const UNRESOLVABLE: &str = "unresolvable:";

/// The identifier of `asset` authored in the layer at `anchor` (an identifier itself).
pub(super) fn identifier(asset: &str, anchor: Option<&str>) -> String {
    let asset = asset.trim();
    if asset.is_empty() {
        return String::new();
    }
    if asset.contains("://") || asset.starts_with('`') {
        return format!("{UNRESOLVABLE}{asset}");
    }
    let asset = asset.replace('\\', "/");
    if ar::is_package_relative_path(&asset) {
        return match ar::split_package_relative_path_outer(&asset) {
            Some((package, inner)) if !package.contains('[') => {
                let anchored = identifier(&package, anchor);
                if anchored.starts_with(UNRESOLVABLE) {
                    return anchored;
                }
                match normalize(&inner) {
                    Some(inner) => ar::join_package_relative_path(&anchored, &inner),
                    None => format!("{UNRESOLVABLE}{asset}"),
                }
            }
            _ => format!("{UNRESOLVABLE}{asset}"),
        };
    }
    let bytes = asset.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if drive {
        return format!("{UNRESOLVABLE}{asset}");
    }
    if asset.starts_with('/') {
        // Without an anchor the caller is opening a layer by its identifier (the stage opening
        // its root); an absolute path authored in a layer names a place on the author's
        // machine and never resolves.
        return match (anchor, normalize(&asset)) {
            (None, Some(joined)) => format!("/{joined}"),
            _ => format!("{UNRESOLVABLE}{asset}"),
        };
    }
    match anchor {
        Some(anchor) if ar::is_package_relative_path(anchor) => {
            let Some((package, inner)) = ar::split_package_relative_path_inner(anchor) else {
                return format!("{UNRESOLVABLE}{asset}");
            };
            let dir = inner.rsplit_once('/').map_or("", |(dir, _)| dir);
            match normalize(&format!("{dir}/{asset}")) {
                Some(joined) => ar::join_package_relative_path(&package, &joined),
                None => format!("{UNRESOLVABLE}{asset}"),
            }
        }
        Some(anchor) if anchor.to_ascii_lowercase().ends_with(".usdz") => match normalize(&asset) {
            Some(joined) => ar::join_package_relative_path(anchor, &joined),
            None => format!("{UNRESOLVABLE}{asset}"),
        },
        Some(anchor) => {
            let dir = anchor.rsplit_once('/').map_or("", |(dir, _)| dir);
            match normalize(&format!("{dir}/{asset}")) {
                Some(joined) => format!("/{joined}"),
                None => format!("{UNRESOLVABLE}{asset}"),
            }
        }
        None => match normalize(&asset) {
            Some(joined) => format!("/{joined}"),
            None => format!("{UNRESOLVABLE}{asset}"),
        },
    }
}

/// The bytes of `inner` inside the package bytes `package`.
fn package_entry(package: &Arc<[u8]>, inner: &str) -> Result<AssetBuffer, String> {
    let mut archive = usdz::Archive::from_bytes(AssetBuffer::Shared(SharedBuffer::new(package.clone())))
        .map_err(|e| e.to_string())?;
    archive.entry(inner).map_err(|e| e.to_string())
}

/// A resolver serving only the walked files.
#[derive(Clone)]
pub(super) struct VfsResolver {
    files: Arc<FileMap>,
}

impl ar::Resolver for VfsResolver {
    fn create_identifier(&self, asset_path: &str, anchor: Option<&ResolvedPath>) -> String {
        let anchor = anchor.map(|a| a.to_string_lossy().into_owned());
        identifier(asset_path, anchor.as_deref())
    }

    fn resolve(&self, asset_path: &str) -> Option<ResolvedPath> {
        if asset_path.is_empty() || asset_path.starts_with(UNRESOLVABLE) {
            return None;
        }
        if ar::is_package_relative_path(asset_path) {
            let (package, inner) = ar::split_package_relative_path_outer(asset_path)?;
            let bytes = self.files.get(&package)?;
            let archive =
                usdz::Archive::from_bytes(AssetBuffer::Shared(SharedBuffer::new(bytes.clone()))).ok()?;
            return archive.contains(&inner).then(|| ResolvedPath::new(asset_path));
        }
        self.files
            .contains_key(asset_path)
            .then(|| ResolvedPath::new(asset_path))
    }

    fn resolve_for_new_asset(&self, _asset_path: &str) -> Option<ResolvedPath> {
        None
    }

    fn open_asset(&self, resolved_path: &ResolvedPath) -> io::Result<Box<dyn ar::Asset>> {
        let id = resolved_path.to_string_lossy();
        if ar::is_package_relative_path(&id) {
            let (package, inner) = ar::split_package_relative_path_outer(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad package path"))?;
            let bytes = usdz::package_entry(self, &package, &inner)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            return Ok(Box::new(io::Cursor::new(bytes)));
        }
        let bytes = self
            .files
            .get(id.as_ref())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{id} is not in the import")))?;
        Ok(Box::new(io::Cursor::new(AssetBuffer::Shared(SharedBuffer::new(
            bytes.clone(),
        )))))
    }
}

/// The file kinds the walk tells apart by their first bytes.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"#usda") {
        Some("usda")
    } else if bytes.starts_with(b"PXR-USDC") {
        Some("usdc")
    } else if bytes.starts_with(b"PK\x03\x04") {
        Some("usdz")
    } else {
        None
    }
}

/// Refuses a text layer whose brackets nest deeper than `limit` (the text parser recurses).
fn check_text_nesting(bytes: &[u8], limit: usize, id: &str) -> Result<(), UsdError> {
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for &b in bytes {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return Err(UsdError::Limit(format!(
                        "{}: brackets nest deeper than {limit}",
                        display(id)
                    )));
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// The vetted files of one import and what the walk found.
pub(super) struct LayerSet {
    /// The identifier the stage opens.
    pub root_id: String,
    /// Every file read, by identifier.
    pub files: Arc<FileMap>,
    /// Layers read, root first (display form).
    pub layers: Vec<String>,
    /// The root file's encoding.
    pub format: &'static str,
    /// Problems that did not stop the walk.
    pub warnings: Vec<String>,
}

impl LayerSet {
    /// Walks the layers of the file at `path`.
    pub(super) fn walk_file(path: &Path, limits: &UsdLimits) -> Result<Self, UsdError> {
        let root_dir = super::import_root(path);
        let canonical = path
            .canonicalize()
            .map_err(|e| UsdError::Io(format!("{}: {e}", path.display())))?;
        let rel = canonical
            .strip_prefix(&root_dir)
            .map_err(|_| UsdError::Io(format!("{} is outside its import root", path.display())))?;
        let rel: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let root_id = format!("/{}", rel.join("/"));
        let mut walker = Walker::new(limits, Some(root_dir));
        walker.run(root_id)
    }

    /// Walks in-memory bytes named `name`: nothing else is readable.
    pub(super) fn walk_bytes(name: &str, bytes: &[u8], limits: &UsdLimits) -> Result<Self, UsdError> {
        if bytes.len() as u64 > limits.max_file_bytes {
            return Err(UsdError::TooLarge {
                size: bytes.len() as u64,
                limit: limits.max_file_bytes,
            });
        }
        let clean: String = name
            .chars()
            .map(|c| {
                if c == '/' || c == '\\' || c == '[' || c == ']' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let root_id = format!(
            "/{}",
            if clean.is_empty() {
                "inline.usda".into()
            } else {
                clean
            }
        );
        let mut walker = Walker::new(limits, None);
        walker.total = bytes.len() as u64;
        walker.files.insert(root_id.clone(), Arc::from(bytes));
        walker.run(root_id)
    }

    /// A resolver over the set.
    pub(super) fn resolver(&self) -> VfsResolver {
        VfsResolver {
            files: self.files.clone(),
        }
    }

    /// The bytes of the file `id` names (a packaged file read from its package), if the walk
    /// read it.
    pub(super) fn asset_bytes(&self, id: &str) -> Option<Vec<u8>> {
        if ar::is_package_relative_path(id) {
            let (package, inner) = ar::split_package_relative_path_outer(id)?;
            if inner.contains('[') {
                return None;
            }
            let bytes = self.files.get(&package)?;
            return package_entry(bytes, &inner).ok().map(|b| b.as_ref().to_vec());
        }
        self.files.get(id).map(|b| b.to_vec())
    }

    /// Digest of every file in the set, by identifier and content.
    pub(super) fn inputs_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"usd-inputs-1");
        for (id, bytes) in self.files.iter() {
            hasher.update(b";");
            hasher.update(id.as_bytes());
            hasher.update(b"=");
            hasher.update(blake3::hash(bytes).as_bytes());
        }
        *hasher.finalize().as_bytes()
    }
}

/// The depth-first walk.
struct Walker<'a> {
    limits: &'a UsdLimits,
    /// The import root (canonical) on disk; `None` for in-memory bytes.
    root_dir: Option<PathBuf>,
    files: FileMap,
    total: u64,
    /// Layers fully walked.
    done: BTreeSet<String>,
    /// Identifiers looked for and not found (warned once each).
    missing: BTreeSet<String>,
    layers: Vec<String>,
    warnings: Vec<String>,
}

impl<'a> Walker<'a> {
    fn new(limits: &'a UsdLimits, root_dir: Option<PathBuf>) -> Self {
        Self {
            limits,
            root_dir,
            files: FileMap::new(),
            total: 0,
            done: BTreeSet::new(),
            missing: BTreeSet::new(),
            layers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn run(&mut self, root_id: String) -> Result<LayerSet, UsdError> {
        let root = self
            .load_host(&root_id)?
            .ok_or_else(|| UsdError::Io(format!("{} not found", display(&root_id))))?;
        let format = sniff(&root).ok_or_else(|| {
            UsdError::NotUsd(format!(
                "{} starts with neither `#usda`, `PXR-USDC` nor a zip header",
                display(&root_id)
            ))
        })?;
        let mut stack = Vec::new();
        self.visit(&root_id, &mut stack)?;
        Ok(LayerSet {
            root_id,
            files: Arc::new(std::mem::take(&mut self.files)),
            layers: std::mem::take(&mut self.layers),
            format,
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < 256 {
            self.warnings.push(message);
        }
    }

    /// Reads the host file `id` names (inside the import root), once.
    fn load_host(&mut self, id: &str) -> Result<Option<Arc<[u8]>>, UsdError> {
        if let Some(bytes) = self.files.get(id) {
            return Ok(Some(bytes.clone()));
        }
        let Some(root) = &self.root_dir else {
            return Ok(None);
        };
        if id.starts_with(UNRESOLVABLE) || !id.starts_with('/') {
            return Ok(None);
        }
        let path = root.join(id.trim_start_matches('/'));
        if !path.is_file() {
            return Ok(None);
        }
        // Symlinks resolved, the file must stay inside the import root.
        let Ok(real) = path.canonicalize() else {
            return Ok(None);
        };
        if !real.starts_with(root) {
            self.warn(format!("{} leads outside the import root; not read", display(id)));
            return Ok(None);
        }
        let bytes = read_capped(&real, self.limits.max_file_bytes)?;
        self.total += bytes.len() as u64;
        if self.total > self.limits.max_total_bytes {
            return Err(UsdError::TooLarge {
                size: self.total,
                limit: self.limits.max_total_bytes,
            });
        }
        let bytes: Arc<[u8]> = Arc::from(bytes);
        self.files.insert(id.to_string(), bytes.clone());
        Ok(Some(bytes))
    }

    /// The bytes of a layer or texture `id` names: a host file, or an entry of a walked package.
    fn load(&mut self, id: &str) -> Result<Option<Vec<u8>>, UsdError> {
        if ar::is_package_relative_path(id) {
            let Some((package, inner)) = ar::split_package_relative_path_outer(id) else {
                return Ok(None);
            };
            if inner.contains('[') {
                self.warn(format!(
                    "{}: packages nested in packages are not read",
                    display(id)
                ));
                return Ok(None);
            }
            let Some(bytes) = self.load_host(&package)? else {
                return Ok(None);
            };
            return Ok(package_entry(&bytes, &inner).ok().map(|b| b.as_ref().to_vec()));
        }
        Ok(self.load_host(id)?.map(|b| b.to_vec()))
    }

    fn visit(&mut self, id: &str, stack: &mut Vec<String>) -> Result<(), UsdError> {
        if let Some(at) = stack.iter().position(|s| s == id) {
            let mut chain: Vec<String> = stack[at..].iter().map(|s| display(s)).collect();
            chain.push(display(id));
            return Err(UsdError::ReferenceCycle(chain.join(" -> ")));
        }
        if stack.len() > self.limits.max_reference_depth {
            let mut chain: Vec<String> = stack.iter().map(|s| display(s)).collect();
            chain.push(display(id));
            return Err(UsdError::ReferenceTooDeep {
                limit: self.limits.max_reference_depth,
                chain: chain.join(" -> "),
            });
        }
        if self.done.contains(id) {
            return Ok(());
        }
        let Some(bytes) = self.load(id)? else {
            if self.missing.insert(id.to_string()) {
                self.warn(format!("layer {} not found; its arcs are empty", display(id)));
            }
            return Ok(());
        };
        // A package's layer is its first `.usd*` entry; its paths anchor inside the package.
        let (anchor, layer_bytes) = if sniff(&bytes) == Some("usdz") && !ar::is_package_relative_path(id) {
            let package: Arc<[u8]> = Arc::from(bytes);
            let archive = usdz::Archive::from_bytes(AssetBuffer::Shared(SharedBuffer::new(package.clone())))
                .map_err(|e| UsdError::Parse(format!("{}: {e}", display(id))))?;
            let first = archive
                .first_layer_name()
                .ok_or_else(|| UsdError::Parse(format!("{}: the package has no layer", display(id))))?;
            let layer = package_entry(&package, &first)
                .map_err(|e| UsdError::Parse(format!("{}: {e}", display(id))))?;
            (
                ar::join_package_relative_path(id, &first),
                layer.as_ref().to_vec(),
            )
        } else {
            (id.to_string(), bytes)
        };
        match sniff(&layer_bytes) {
            Some("usda") => check_text_nesting(&layer_bytes, self.limits.max_text_nesting, id)?,
            Some("usdc") => {}
            Some(_) => {
                return Err(UsdError::Parse(format!(
                    "{}: a package inside a package is not read",
                    display(id)
                )));
            }
            None => {
                return Err(UsdError::NotUsd(format!(
                    "{} is not a USD layer",
                    display(&anchor)
                )));
            }
        }
        self.done.insert(id.to_string());
        self.layers.push(display(id));
        if self.layers.len() > self.limits.max_layers {
            return Err(UsdError::Limit(format!(
                "more than {} layers",
                self.limits.max_layers
            )));
        }
        let mut layer = sdf::Layer::from_bytes(display(&anchor), layer_bytes)
            .map_err(|e| UsdError::Parse(format!("{}: {e}", display(&anchor))))?;
        let mut authored: Vec<String> = Vec::new();
        usd_utils::modify_asset_paths(
            &mut layer,
            |path| {
                if authored.len() < 1 << 16 {
                    authored.push(path.to_string());
                }
                path.to_string()
            },
            true,
        )
        .map_err(|e| UsdError::Parse(format!("{}: {e}", display(&anchor))))?;
        stack.push(id.to_string());
        for path in authored {
            let child = identifier(&path, Some(&anchor));
            if child.is_empty() {
                continue;
            }
            if child.starts_with(UNRESOLVABLE) {
                if self.missing.insert(child.clone()) {
                    self.warn(format!(
                        "asset path @{path}@ is remote, absolute or leaves its root; not read"
                    ));
                }
                continue;
            }
            let ext = extension_of(&child);
            if is_usd_extension(&ext) {
                self.visit(&child, stack)?;
            } else if is_image_extension(&ext) && !self.files.contains_key(&child) {
                // Textures: read now (caps, root), so the digest covers them.
                if self.load(&child)?.is_none() && self.missing.insert(child.clone()) {
                    self.warn(format!("texture {} not found", display(&child)));
                }
            }
        }
        stack.pop();
        Ok(())
    }
}

/// Reads a file refusing one larger than `limit` bytes.
fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>, UsdError> {
    use std::io::Read;
    let file = crate::mesh_convert::open_regular(path)
        .map_err(|e| UsdError::Io(format!("{}: {e}", path.display())))?;
    let size = file
        .metadata()
        .map_err(|e| UsdError::Io(format!("{}: {e}", path.display())))?
        .len();
    if size > limit {
        return Err(UsdError::TooLarge { size, limit });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| UsdError::Io(format!("{}: {e}", path.display())))?;
    if bytes.len() as u64 > limit {
        return Err(UsdError::TooLarge {
            size: bytes.len() as u64,
            limit,
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_anchor_normalize_and_refuse_escapes() {
        assert_eq!(identifier("b.usda", Some("/a/root.usda")), "/a/b.usda");
        assert_eq!(identifier("./sub/../c.usda", Some("/a/root.usda")), "/a/c.usda");
        assert_eq!(identifier("../c.usda", Some("/a/root.usda")), "/c.usda");
        assert!(identifier("../../c.usda", Some("/a/root.usda")).starts_with(UNRESOLVABLE));
        assert!(identifier("/etc/passwd", Some("/a/root.usda")).starts_with(UNRESOLVABLE));
        assert!(identifier("C:/x.usda", None).starts_with(UNRESOLVABLE));
        assert_eq!(
            identifier("/a/./root.usda", None),
            "/a/root.usda",
            "the opener's own identifier"
        );
        assert!(identifier("https://host/x.usda", None).starts_with(UNRESOLVABLE));
        assert_eq!(
            identifier("tex/wood.png", Some("/m/chair.usdz[chair.usda]")),
            "/m/chair.usdz[tex/wood.png]"
        );
        assert_eq!(
            identifier("tex/wood.png", Some("/m/chair.usdz")),
            "/m/chair.usdz[tex/wood.png]"
        );
        assert!(identifier("../../x.png", Some("/m/chair.usdz[a/chair.usda]")).starts_with(UNRESOLVABLE));
        assert_eq!(
            identifier("chair.usdz[x.usda]", Some("/m/root.usda")),
            "/m/chair.usdz[x.usda]"
        );
        assert_eq!(identifier("", None), "");
        assert_eq!(extension_of("/m/chair.usdz[tex/Wood.PNG]"), "png");
        assert_eq!(extension_of("/m/root.usda"), "usda");
    }

    #[test]
    fn text_nesting_is_capped_outside_strings() {
        let deep = format!("#usda 1.0\n{}{}", "(".repeat(200), ")".repeat(200));
        assert_eq!(
            check_text_nesting(deep.as_bytes(), 128, "/x.usda").map_err(|e| e.code()),
            Err("usd_limit")
        );
        let quoted = format!("#usda 1.0\n\"{}\"", "(".repeat(200));
        assert!(check_text_nesting(quoted.as_bytes(), 128, "/x.usda").is_ok());
    }
}
