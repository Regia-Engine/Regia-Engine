// SPDX-License-Identifier: MPL-2.0
//! The `engine://` asset source (ADR-7232): files the engine ships in code, under paths such as
//! `engine://net/profiles/player.netprofile.ron`.
//!
//! Engine crates register their built-in files in the [`EngineAssets`] resource when their plugin
//! builds (the asset plugin inserts an empty one). Project content comes from the cooked cache or
//! the mounted paks through the [`AssetServer`]. [`read_text_asset`] reads either kind by path, so a
//! system that loads, say, net profiles handles the built-in ones and the project's the same way;
//! [`text_assets_with_suffix`] lists both.
//!
//! ```
//! use regia_asset::engine_source::{EngineAssets, TextSource, read_text_asset};
//!
//! let mut engine = EngineAssets::default();
//! engine.insert("engine://net/profiles/default.netprofile.ron", "NetProfile()").expect("valid path");
//! let (text, source) =
//!     read_text_asset(&engine, None, "engine://net/profiles/default.netprofile.ron", "regia_net::NetProfile")
//!         .expect("reads");
//! assert_eq!((text.as_str(), source), ("NetProfile()", TextSource::Engine));
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;

use crate::{AssetLoadError, AssetServer, AssetSource};

/// The scheme prefix of engine paths.
pub const ENGINE_SCHEME: &str = "engine://";

/// Whether `path` names the engine source (`engine://...`).
#[must_use]
pub fn is_engine_path(path: &str) -> bool {
    path.starts_with(ENGINE_SCHEME)
}

/// The built-in files of the `engine://` source, by path relative to `engine://`.
#[derive(Resource, Debug, Default, Clone)]
pub struct EngineAssets {
    files: BTreeMap<String, Arc<str>>,
}

/// Why a path cannot be an engine path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{path}` is not a valid engine:// path: {reason}")]
pub struct EnginePathError {
    /// The rejected path.
    pub path: String,
    /// Why.
    pub reason: &'static str,
}

fn relative(path: &str) -> Result<&str, EnginePathError> {
    let error = |reason| EnginePathError {
        path: path.to_owned(),
        reason,
    };
    let rel = path.strip_prefix(ENGINE_SCHEME).unwrap_or(path);
    if rel.is_empty() {
        return Err(error("empty"));
    }
    if rel.starts_with('/') || rel.contains('\\') || rel.contains("://") {
        return Err(error("absolute, backslash or another scheme"));
    }
    if rel.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(error("empty, `.` or `..` segment"));
    }
    Ok(rel)
}

impl EngineAssets {
    /// Adds or replaces the text file at `path` (`engine://a/b.ron` or `a/b.ron`).
    ///
    /// # Errors
    ///
    /// The path is empty, absolute, has an empty, `.` or `..` segment, a backslash or another scheme.
    pub fn insert(&mut self, path: &str, text: impl Into<Arc<str>>) -> Result<(), EnginePathError> {
        let rel = relative(path)?;
        self.files.insert(rel.to_owned(), text.into());
        Ok(())
    }

    /// The text at `path` (`engine://a/b.ron` or `a/b.ron`).
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&str> {
        let rel = relative(path).ok()?;
        self.files.get(rel).map(|t| &**t)
    }

    /// Full `engine://` paths of every file whose name ends with `suffix` (ASCII case ignored),
    /// sorted.
    #[must_use]
    pub fn paths_with_suffix(&self, suffix: &str) -> Vec<String> {
        let suffix = suffix.to_ascii_lowercase();
        self.files
            .keys()
            .filter(|p| p.to_ascii_lowercase().ends_with(&suffix))
            .map(|p| format!("{ENGINE_SCHEME}{p}"))
            .collect()
    }

    /// How many files are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether none is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Where a text asset came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSource {
    /// The `engine://` source.
    Engine,
    /// A mounted pak (its name).
    Pak(String),
    /// The development cooked cache.
    DevCache,
}

impl TextSource {
    /// `engine`, `pak` or `cache`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Pak(_) => "pak",
            Self::DevCache => "cache",
        }
    }
}

/// Reads the text asset `path`: an `engine://` path from `engine`, any other path through `server`
/// (its cooked file must be tagged `type_path`, the cooked type of a text-cooked kind such as
/// `regia_net::NetProfile` or [`crate::data_asset::DATA_ASSET_TYPE_PATH`]).
///
/// # Errors
///
/// [`AssetLoadError::Io`] (`NotFound`) for an unknown path or a project path with no server; the
/// errors of [`crate::data_asset::cooked_text`].
pub fn read_text_asset(
    engine: &EngineAssets,
    server: Option<&mut AssetServer>,
    path: &str,
    type_path: &str,
) -> Result<(String, TextSource), AssetLoadError> {
    let not_found = |why: &str| {
        AssetLoadError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{path}: {why}"),
        ))
    };
    if is_engine_path(path) {
        return engine
            .get(path)
            .map(|t| (t.to_owned(), TextSource::Engine))
            .ok_or_else(|| not_found("no such engine:// file"));
    }
    let server = server.ok_or_else(|| not_found("no asset server"))?;
    let id = server
        .id_for_path(path)
        .ok_or_else(|| not_found("not in the asset manifest"))?;
    let (bytes, source) = server.read_cooked(id)?;
    let text = crate::data_asset::cooked_text(&bytes, type_path)?.to_owned();
    let source = match source {
        AssetSource::Pak(name) => TextSource::Pak(name),
        AssetSource::DevCache | AssetSource::Unknown => TextSource::DevCache,
    };
    Ok((text, source))
}

/// Every text asset whose name ends with `suffix`: the `engine://` files first (sorted), then the
/// project's manifest paths (sorted).
#[must_use]
pub fn text_assets_with_suffix(
    engine: &EngineAssets,
    server: Option<&AssetServer>,
    suffix: &str,
) -> Vec<String> {
    let mut out = engine.paths_with_suffix(suffix);
    if let Some(server) = server {
        out.extend(server.paths_with_suffix(suffix));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_paths_are_confined_and_listed() {
        let mut engine = EngineAssets::default();
        for bad in [
            "",
            "engine://",
            "/etc/passwd",
            "engine://../x",
            "a//b",
            "a\\b",
            "file://x",
            "a/./b",
        ] {
            assert!(engine.insert(bad, "x").is_err(), "{bad:?} must be refused");
        }
        engine
            .insert("engine://net/profiles/b.netprofile.ron", "B")
            .expect("ok");
        engine.insert("net/profiles/a.NetProfile.ron", "A").expect("ok");
        engine.insert("net/other.ron", "O").expect("ok");
        assert_eq!(engine.get("net/profiles/b.netprofile.ron"), Some("B"));
        assert_eq!(engine.get("engine://net/other.ron"), Some("O"));
        assert_eq!(engine.get("engine://net/../other.ron"), None);
        assert_eq!(
            engine.paths_with_suffix(".netprofile.ron"),
            [
                "engine://net/profiles/a.NetProfile.ron",
                "engine://net/profiles/b.netprofile.ron"
            ]
        );
        assert!(matches!(
            read_text_asset(&engine, None, "net/profiles/x.netprofile.ron", "t"),
            Err(AssetLoadError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(read_text_asset(&engine, None, "engine://missing.ron", "t").is_err());
    }
}
