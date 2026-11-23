// SPDX-License-Identifier: MPL-2.0
//! Where the configurator's documents come from (ADR-2534): a [`ContentSource`] reads a file by
//! its `/`-separated path relative to a content root. The core never touches a file system, a
//! thread or the network, so it builds for the browser and Android unchanged; a host supplies the
//! source (a directory with the `fs` feature, bytes it fetched over HTTP, an Android asset).
//!
//! The paths follow the plan's output layout (`interfaces.md` § 0.5):
//!
//! ```text
//! optiongraph/<snapshot_id>/houses/<house>/products/<product_slug>.json
//! recipes/<house>/<design_slug>.json
//! kits/<house|shared>/<kit_id>/kit.json
//! materials/<library_version>/library.json
//! exchange/<name>/1/schema.json      (an owner's published schema; optional)
//! ```

use std::collections::BTreeMap;

use serde_json::Value;

use crate::Interface;
#[cfg(feature = "validate")]
use crate::Schema;
use crate::content::{MaterialLibrary, PartKit, Recipe};
use crate::graph::{LoadError, ProductRecord, slug};

/// Why a source could not read a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Nothing at that path.
    NotFound(String),
    /// The path exists but could not be read.
    Io(String),
    /// The path is not a clean relative path ([`clean_path`]).
    BadPath(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "`{p}` not found"),
            Self::Io(m) => write!(f, "{m}"),
            // The refused text is hostile by definition: no raw NUL or bidi control in a message.
            Self::BadPath(p) => write!(f, "`{}` is not a clean relative path", p.escape_debug()),
        }
    }
}

impl std::error::Error for SourceError {}

/// Reads documents by relative path.
pub trait ContentSource {
    /// The bytes at `path` (`/`-separated, relative to the root).
    fn read(&self, path: &str) -> Result<Vec<u8>, SourceError>;
}

/// Most bytes of a path [`clean_path`] accepts, and of a content root ([`clean_root`]).
pub const MAX_PATH_BYTES: usize = 1024;

/// Most bytes of one path segment [`clean_path`] accepts (the usual file name limit).
pub const MAX_SEGMENT_BYTES: usize = 255;

/// A Windows device name, which Windows opens whatever the folder and the extension. Refused on
/// every platform so a path means the same everywhere; the same rule as the engine's request paths
/// (ADR-1816), held here because this crate depends on no engine crate.
fn is_device(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment).trim_end_matches(' ');
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|d| stem.eq_ignore_ascii_case(d))
    {
        return true;
    }
    match stem.as_bytes() {
        [a, b, c, d] => {
            let head = [*a, *b, *c];
            (head.eq_ignore_ascii_case(b"COM") || head.eq_ignore_ascii_case(b"LPT")) && d.is_ascii_digit()
        }
        _ => false,
    }
}

/// Whether `path` is a clean relative path, the same on every platform: `/`-separated, with no
/// leading `/`, backslash, `:` (a Windows drive replaces the root it joins), control character
/// (NUL first), or device name, no segment that is empty or only dots and spaces (`.`, `..`, and
/// `.. ` or `...`, which Windows reads as `..`), and within [`MAX_PATH_BYTES`] and
/// [`MAX_SEGMENT_BYTES`].
pub fn clean_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|s| {
            s.len() <= MAX_SEGMENT_BYTES && !s.trim_end_matches([' ', '.']).is_empty() && !is_device(s)
        })
}

/// Whether `root` can name a content directory for `DirSource`: not empty (which would mean the
/// working directory), no control character, within [`MAX_PATH_BYTES`]. A root may be absolute and
/// may use the platform's separators.
pub fn clean_root(root: &str) -> bool {
    !root.is_empty() && root.len() <= MAX_PATH_BYTES && !root.chars().any(char::is_control)
}

/// A source held in memory (tests, and hosts that fetched a product package).
#[derive(Debug, Clone, Default)]
pub struct MemorySource {
    files: BTreeMap<String, Vec<u8>>,
}

impl MemorySource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) one file.
    pub fn insert(&mut self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) -> &mut Self {
        self.files.insert(path.into(), bytes.into());
        self
    }
}

impl ContentSource for MemorySource {
    fn read(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        if !clean_path(path) {
            return Err(SourceError::BadPath(path.to_owned()));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound(path.to_owned()))
    }
}

/// A source rooted at a directory (desktop and Android hosts; the `fs` feature). A path that goes
/// through a symbolic link below the root is refused, so a package folder unpacked from an archive
/// cannot reach a file outside itself; the root itself may be a link.
#[cfg(feature = "fs")]
#[derive(Debug, Clone)]
pub struct DirSource {
    root: std::path::PathBuf,
}

#[cfg(feature = "fs")]
impl DirSource {
    /// A source that reads below `root`.
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[cfg(feature = "fs")]
impl ContentSource for DirSource {
    fn read(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        if !clean_path(path) {
            return Err(SourceError::BadPath(path.to_owned()));
        }
        use std::io::{ErrorKind, Read};
        let full = self.root.join(path);
        // By kind, not by the OS message: the same answer on every platform, nothing about the host.
        let refused = |e: std::io::Error| match e.kind() {
            ErrorKind::NotFound | ErrorKind::NotADirectory => SourceError::NotFound(path.to_owned()),
            kind => SourceError::Io(format!("`{path}` cannot be read: {kind}")),
        };
        // Every step below the root without following links: a link (to a file or a folder) is
        // refused, whatever it points at.
        let mut at = self.root.clone();
        let mut leaf = None;
        for part in path.split('/') {
            at.push(part);
            let meta = std::fs::symlink_metadata(&at).map_err(refused)?;
            if meta.file_type().is_symlink() {
                return Err(SourceError::Io(format!(
                    "`{path}` goes through a link; content is read only from files inside its folder"
                )));
            }
            leaf = Some(meta);
        }
        // Before the open: opening a FIFO blocks, and a device such as /dev/zero never ends.
        let Some(leaf) = leaf.filter(std::fs::Metadata::is_file) else {
            return Err(SourceError::Io(format!("`{path}` is not a regular file")));
        };
        let file = std::fs::File::open(&full).map_err(refused)?;
        // The file opened must be the one checked: a swap between the check and the open (a link put
        // in its place) is refused.
        let opened = file.metadata().map_err(refused)?;
        if !opened.is_file() || !same_file(&leaf, &opened) {
            return Err(SourceError::Io(format!("`{path}` changed while it was read")));
        }
        // One byte past the limit is enough for the loader to refuse the file.
        let mut bytes = Vec::new();
        file.take(crate::MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(refused)?;
        Ok(bytes)
    }
}

/// Whether two metadata name one file: device and inode on Unix (Linux, Android, macOS). Elsewhere the
/// link walk before the open is the check (Windows needs a privilege to make a link at all).
#[cfg(feature = "fs")]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
    }
}

/// What to open: a product key plus where its documents are. Any explicit path wins over the
/// layout default.
#[derive(Debug, Clone, Default)]
pub struct OpenRequest {
    /// `<house>/<house_key>`.
    pub product_key: String,
    /// The option graph snapshot (needed unless `option_graph_path` is given).
    pub snapshot_id: Option<String>,
    /// The material library version (no library when absent and `library_path` is absent).
    pub library_version: Option<String>,
    /// The option graph product record.
    pub option_graph_path: Option<String>,
    /// The recipe (default `recipes/<house>/<design_slug>.json`; a missing default recipe means
    /// the product is configurable but has no look).
    pub recipe_path: Option<String>,
    /// The material library.
    pub library_path: Option<String>,
    /// The part kits (`kits/<house|shared>/<kit_id>/kit.json`).
    pub kit_paths: Vec<String>,
    /// Validate every document against the owners' published schemas found under
    /// `exchange/<name>/1/schema.json`, and the Regia-owned ones (kit, library) against the shipped
    /// contract checks.
    pub validate: bool,
}

impl OpenRequest {
    /// A product package folder (`docs/specs/42-product-viewer.md` § 8.1): `options.json` (the
    /// option graph record, costs stripped), `recipe.json` and `materials.json` at the package root.
    pub fn package(product_key: impl Into<String>) -> Self {
        Self {
            product_key: product_key.into(),
            option_graph_path: Some("options.json".to_owned()),
            recipe_path: Some("recipe.json".to_owned()),
            library_path: Some("materials.json".to_owned()),
            validate: true,
            ..Self::default()
        }
    }
}

/// Everything one product needs.
#[derive(Debug, Clone)]
pub struct ProductContent {
    /// The option graph product record.
    pub product: ProductRecord,
    /// The recipe, when one exists.
    pub recipe: Option<Recipe>,
    /// The part kits.
    pub kits: Vec<PartKit>,
    /// The material library, when one was named.
    pub library: Option<MaterialLibrary>,
    /// Which paths were read, in order.
    pub read: Vec<String>,
}

fn read_json(source: &dyn ContentSource, path: &str) -> Result<(Vec<u8>, Value), LoadError> {
    let bytes = source.read(path).map_err(|e| match e {
        SourceError::NotFound(_) => LoadError {
            reason: "configurator_missing_file",
            message: e.to_string(),
        },
        SourceError::BadPath(_) => LoadError {
            reason: "configurator_bad_path",
            message: e.to_string(),
        },
        SourceError::Io(_) => LoadError {
            reason: "configurator_io",
            message: e.to_string(),
        },
    })?;
    if bytes.len() > crate::MAX_FILE_BYTES {
        return Err(LoadError::bad(format!(
            "`{path}` is larger than {} bytes",
            crate::MAX_FILE_BYTES
        )));
    }
    let value = serde_json::from_slice(&bytes).map_err(|e| LoadError::bad(format!("`{path}`: {e}")))?;
    Ok((bytes, value))
}

/// Validate `value` (read from `path`) against the published schema of `interface` when the source
/// has one; Regia-owned interfaces also run the shipped contract checks.
fn validate(
    source: &dyn ContentSource,
    interface: Interface,
    path: &str,
    value: &Value,
) -> Result<(), LoadError> {
    // Without the validator no published schema can be compiled, so the shipped contract checks stand alone.
    #[cfg(not(feature = "validate"))]
    let published: Option<crate::Schema> = {
        let _ = source;
        None
    };
    #[cfg(feature = "validate")]
    let schema_path = format!("exchange/{}/1/schema.json", interface.wire_name());
    #[cfg(feature = "validate")]
    let published = match source.read(&schema_path) {
        Ok(bytes) => {
            // The same cap as every other document of a source, before parsing.
            if bytes.len() > crate::MAX_FILE_BYTES {
                return Err(LoadError::bad(format!(
                    "`{schema_path}` is larger than {} bytes",
                    crate::MAX_FILE_BYTES
                )));
            }
            let doc: Value = serde_json::from_slice(&bytes)
                .map_err(|e| LoadError::bad(format!("`{schema_path}`: {e}")))?;
            Some(Schema::compile(doc).map_err(|e| LoadError {
                reason: "configurator_contract",
                message: format!("`{schema_path}` cannot be used: {e}"),
            })?)
        }
        Err(SourceError::NotFound(_)) => None,
        Err(e) => return Err(LoadError::bad(e.to_string())),
    };
    if published.is_none() && !interface.owned_by_regia() {
        // Nothing to validate a consumed document against until its owner publishes a schema.
        return Ok(());
    }
    crate::check(value, Some(interface), published.as_ref())
        .map(|_| ())
        .map_err(|errors| LoadError {
            reason: "configurator_contract",
            message: format!(
                "`{path}` breaks the {} contract: {}",
                interface.wire_name(),
                errors
                    .iter()
                    .take(5)
                    .map(|e| format!("{} at `{}`", e.reason, e.path))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        })
}

/// Load one product's documents from `source` (module docs for the layout).
pub fn load_product(source: &dyn ContentSource, request: &OpenRequest) -> Result<ProductContent, LoadError> {
    let mut read = Vec::new();
    let Some((house, _)) = request.product_key.split_once('/') else {
        return Err(LoadError::bad(format!(
            "product_key `{}` is not `<house>/<key>`",
            request.product_key
        )));
    };
    let graph_path = match (&request.option_graph_path, &request.snapshot_id) {
        (Some(p), _) => p.clone(),
        (None, Some(snap)) => format!(
            "optiongraph/{snap}/houses/{house}/products/{}.json",
            slug(&request.product_key)
        ),
        (None, None) => {
            return Err(LoadError::bad(
                "a snapshot_id or an option_graph path is required",
            ));
        }
    };
    let (_, value) = read_json(source, &graph_path)?;
    if request.validate {
        validate(source, Interface::OptionGraph, &graph_path, &value)?;
    }
    let product = ProductRecord::from_value(value)?;
    if product.product_key != request.product_key {
        return Err(LoadError::bad(format!(
            "`{graph_path}` holds `{}`, not `{}`",
            product.product_key, request.product_key
        )));
    }
    read.push(graph_path);

    let default_recipe = request.recipe_path.is_none();
    let recipe_path = request
        .recipe_path
        .clone()
        .unwrap_or_else(|| format!("recipes/{house}/{}.json", slug(product.design_key())));
    let recipe = match read_json(source, &recipe_path) {
        Ok((_, value)) => {
            if request.validate {
                validate(source, Interface::PieceRecipe, &recipe_path, &value)?;
            }
            let recipe = Recipe::from_value(value)?;
            if recipe.design_key != product.design_key() {
                return Err(LoadError::bad(format!(
                    "`{recipe_path}` is the recipe of `{}`, not of `{}`",
                    recipe.design_key,
                    product.design_key()
                )));
            }
            if !recipe.house.is_empty() && recipe.house != house {
                return Err(LoadError {
                    reason: "configurator_house_not_allowed",
                    message: format!("`{recipe_path}` belongs to `{}`, not `{house}`", recipe.house),
                });
            }
            // spec 42 § 5.1: a drive of an option group the product does not have is
            // refused, not skipped silently at look time.
            for drive in &recipe.drives {
                if product.group(&drive.group).is_none() {
                    return Err(LoadError {
                        reason: "recipe_drive_unknown",
                        message: format!(
                            "`{recipe_path}` drives `{}`, which `{}` has no option group for",
                            drive.group, product.product_key
                        ),
                    });
                }
            }
            read.push(recipe_path);
            Some(recipe)
        }
        Err(e) if default_recipe && e.reason == "configurator_missing_file" => None,
        Err(e) => return Err(e),
    };

    let mut kits = Vec::new();
    for path in &request.kit_paths {
        let (bytes, value) = read_json(source, path)?;
        if request.validate {
            validate(source, Interface::PartKit, path, &value)?;
        }
        let kit = PartKit::from_json(&bytes)?;
        if kit.house != house && kit.house != "shared" {
            return Err(LoadError {
                reason: "configurator_house_not_allowed",
                message: format!(
                    "kit `{}` belongs to `{}`; `{house}` may load its own kits and shared ones",
                    kit.kit_id, kit.house
                ),
            });
        }
        read.push(path.clone());
        kits.push(kit);
    }

    let library_path = request.library_path.clone().or_else(|| {
        request
            .library_version
            .as_ref()
            .map(|v| format!("materials/{v}/library.json"))
    });
    let library = match library_path {
        Some(path) => {
            let (bytes, value) = read_json(source, &path)?;
            if request.validate {
                validate(source, Interface::MaterialLibrary, &path, &value)?;
            }
            let lib = MaterialLibrary::from_json(&bytes)?;
            read.push(path);
            Some(lib)
        }
        None => None,
    };
    Ok(ProductContent {
        product,
        recipe,
        kits,
        library,
        read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_paths_only() {
        assert!(clean_path("recipes/house-a/d1.json"));
        for bad in ["", "/etc/passwd", "a/../b", "./a", "a//b", "a\\b"] {
            assert!(!clean_path(bad), "{bad}");
        }
        let src = MemorySource::new();
        assert!(matches!(src.read("../x"), Err(SourceError::BadPath(_))));
        assert!(matches!(src.read("x.json"), Err(SourceError::NotFound(_))));
    }

    #[test]
    fn hostile_paths_are_not_clean() {
        let long_segment = "z".repeat(256);
        let long_path = "a/".repeat(600) + "a";
        let bad = [
            "\u{0}",
            "a\u{0}b",
            "a/\u{0}",
            "a\nb",
            "a\u{7f}b",
            "C:/x",
            "C:x",
            "a:b",
            "...",
            "a/.. /b",
            "a/. ./b",
            "NUL",
            "a/con.txt",
            "COM1/x",
            "lpt9.json",
            "CONIN$",
            long_segment.as_str(),
            long_path.as_str(),
        ];
        for path in bad {
            assert!(!clean_path(path), "{path:?}");
            let src = MemorySource::new();
            assert!(matches!(src.read(path), Err(SourceError::BadPath(_))), "{path:?}");
        }
        let longest_segment = "z".repeat(255);
        for path in [
            longest_segment.as_str(),
            ".hidden/x.json",
            "comet/console.json",
            "com/lpt/com12",
            "houses/é/x.json",
        ] {
            assert!(clean_path(path), "{path:?}");
        }
    }

    #[test]
    fn a_root_may_be_absolute_but_not_hostile() {
        let too_long = "z".repeat(MAX_PATH_BYTES + 1);
        for root in ["", "\u{0}", "out\u{0}/x", "out\nx", too_long.as_str()] {
            assert!(!clean_root(root), "{root:?}");
        }
        for root in ["/srv/jewelry/out", "out", "C:\\jewelry\\out", "/srv/é x"] {
            assert!(clean_root(root), "{root:?}");
        }
    }

    #[test]
    fn a_refused_path_is_escaped_in_its_message() {
        let text = SourceError::BadPath("a\u{0}\u{202e}b".to_owned()).to_string();
        assert!(!text.contains('\u{0}') && !text.contains('\u{202e}'), "{text}");
    }

    /// Audit fix (sprint-fix-audit-jw, finding 12): a link inside a content folder, to a file or to a
    /// folder outside it, is refused; plain files and a linked root still read.
    #[cfg(all(feature = "fs", unix))]
    #[test]
    fn a_dir_source_refuses_links_below_its_root() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "regia-configurator-links-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let (root, outside) = (base.join("pkg"), base.join("outside"));
        std::fs::create_dir_all(root.join("sub")).expect("dirs");
        std::fs::create_dir_all(&outside).expect("dirs");
        std::fs::write(root.join("sub/ok.json"), b"{}").expect("write");
        std::fs::write(outside.join("secret.json"), b"secret").expect("write");
        std::os::unix::fs::symlink(outside.join("secret.json"), root.join("link.json")).expect("link");
        std::os::unix::fs::symlink(&outside, root.join("away")).expect("link");
        std::os::unix::fs::symlink(&root, base.join("root-link")).expect("link");

        let src = DirSource::new(&root);
        assert_eq!(src.read("sub/ok.json").expect("plain file"), b"{}");
        for linked in ["link.json", "away/secret.json"] {
            match src.read(linked) {
                Err(SourceError::Io(m)) => assert!(m.contains("link"), "{linked}: {m}"),
                other => panic!("{linked}: {other:?}"),
            }
        }
        assert!(
            matches!(src.read("sub"), Err(SourceError::Io(_))),
            "a folder is not a file"
        );
        assert!(matches!(src.read("nope.json"), Err(SourceError::NotFound(_))));
        let via_link = DirSource::new(base.join("root-link"));
        assert_eq!(via_link.read("sub/ok.json").expect("a linked root"), b"{}");
        std::fs::remove_dir_all(&base).ok();
    }

    /// Audit LOW (sprint-corr-configurator): a published schema was parsed whatever its size;
    /// every other document of a source is refused past `MAX_FILE_BYTES` before parsing.
    #[cfg(feature = "validate")]
    #[test]
    fn a_published_schema_past_the_file_cap_is_refused_before_parsing() {
        let mut huge = vec![b' '; crate::MAX_FILE_BYTES + 1];
        huge.extend_from_slice(b"{}");
        let mut src = MemorySource::new();
        src.insert("exchange/option_graph/1/schema.json", huge);
        let doc = serde_json::json!({"schema": "jewelry.option_graph/1"});
        let err = match validate(&src, Interface::OptionGraph, "g.json", &doc) {
            Err(e) => e,
            Ok(()) => panic!("an oversize schema was used"),
        };
        assert!(err.message.contains("larger than"), "{err:?}");
    }
}
