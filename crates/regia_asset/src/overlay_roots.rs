// SPDX-License-Identifier: MPL-2.0
//! Overlay asset roots at runtime (ADR-8155, ADR-8845).
//!
//! A project may name extra asset roots in `Regia.toml` `[cook.overlay_roots]` `paths` (or
//! `REGIA_COOK_OVERLAY_ROOTS`, which replaces that list on one machine). The project cook fills a
//! missing glTF buffer or image from the earliest of those roots that holds the relative path
//! (`regia_asset_pipeline::overlay_roots`). This module holds the configuration, which the cook,
//! the live cooker and the runtime share, and [`SourceRoots`]: the runtime's lookup of a source
//! the game reads directly (a data table, a curve table, ...) in the same order the cook uses:
//!
//! 1. the project's `assets/` (anything already there wins, as in the cook);
//! 2. the overlay roots that exist, earliest first.
//!
//! A path that leaves its root (a `..`, an absolute path, a drive prefix, or a symlink whose
//! target lies outside the overlay root) is not served from an overlay root. Nothing is searched
//! when no roots are configured, and no machine default directories are invented.
//!
//! The behavior is a requirements rewrite of a project-cook payload fill (see
//! `docs/provenance.md`). This module is original Rust.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable that replaces the `[cook.overlay_roots]` path list on one machine.
pub const OVERLAY_ROOTS_ENV: &str = "REGIA_COOK_OVERLAY_ROOTS";

/// Overlay directories resolved for one project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayRoots {
    /// Existing directories, earliest first.
    pub paths: Vec<PathBuf>,
    /// Configured strings that were not directories, in list order.
    pub missing: Vec<String>,
    /// True when the table or a non-empty environment list was present.
    pub configured: bool,
}

impl OverlayRoots {
    /// Reads `Regia.toml` and [`OVERLAY_ROOTS_ENV`].
    ///
    /// # Errors
    /// The manifest does not parse, or `[cook.overlay_roots]` is not a `paths` array of
    /// non-empty strings. A missing manifest is not an error: the feature stays off.
    pub fn load(project: &Path) -> Result<Self, String> {
        let manifest = match fs::read_to_string(project.join("Regia.toml")) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Regia.toml: {error}")),
        };
        let env = std::env::var(OVERLAY_ROOTS_ENV).ok();
        Self::resolve(project, manifest.as_deref(), env.as_deref())
    }

    /// [`Self::load`] with the manifest text and the environment value given.
    ///
    /// A non-empty `env` (after trimming) replaces the table's `paths`. Whitespace only is
    /// treated as unset. Relative paths resolve against `project`. A path that is not a
    /// directory is listed in [`Self::missing`] and skipped, not an error.
    ///
    /// # Errors
    /// As [`Self::load`].
    pub fn resolve(project: &Path, manifest: Option<&str>, env: Option<&str>) -> Result<Self, String> {
        let mut listed = Vec::new();
        let mut configured = false;
        if let Some(text) = manifest {
            listed = paths_from_manifest(text, &mut configured)?;
        }
        if let Some(raw) = env.map(str::trim).filter(|value| !value.is_empty()) {
            configured = true;
            listed = std::env::split_paths(raw)
                .filter(|path| !path.as_os_str().is_empty())
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
        }
        if !configured {
            return Ok(Self::default());
        }
        let mut paths = Vec::new();
        let mut missing = Vec::new();
        for raw in listed {
            let path = PathBuf::from(&raw);
            let full = if path.is_absolute() {
                path
            } else {
                project.join(path)
            };
            if full.is_dir() {
                paths.push(full);
            } else {
                missing.push(raw);
            }
        }
        Ok(Self {
            paths,
            missing,
            configured: true,
        })
    }
}

/// `paths` from `[cook.overlay_roots]`, and whether that table is present.
///
/// # Errors
/// The file does not parse, or the table is not a `paths` array of non-empty strings.
fn paths_from_manifest(text: &str, configured: &mut bool) -> Result<Vec<String>, String> {
    let root: toml::Table = text.parse().map_err(|error| format!("Regia.toml: {error}"))?;
    let Some(overlay) = root
        .get("cook")
        .and_then(toml::Value::as_table)
        .and_then(|cook| cook.get("overlay_roots"))
    else {
        return Ok(Vec::new());
    };
    *configured = true;
    let overlay = overlay
        .as_table()
        .ok_or("Regia.toml: [cook.overlay_roots] must be a table")?;
    for key in overlay.keys() {
        if key != "paths" {
            return Err(format!(
                "Regia.toml: [cook.overlay_roots] has unknown key `{key}` (known: paths)"
            ));
        }
    }
    match overlay.get("paths") {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| match item {
                toml::Value::String(path) if !path.trim().is_empty() => Ok(path.trim().to_string()),
                _ => Err(format!(
                    "Regia.toml: [cook.overlay_roots] paths[{index}] must be a non-empty string"
                )),
            })
            .collect(),
        Some(_) => Err("Regia.toml: [cook.overlay_roots] paths must be an array of strings".to_string()),
    }
}

/// Where [`SourceRoots::resolve`] found a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSource {
    /// The file to open.
    pub path: PathBuf,
    /// `None` when the project's `assets/` holds it, else the index into
    /// [`SourceRoots::overlays`] (the earliest is 0), the same index the cook's
    /// `overlay_fill.json` records.
    pub overlay_index: Option<u32>,
}

/// The project's `assets/` plus its overlay roots, in the order the project cook fills from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceRoots {
    assets: PathBuf,
    overlays: Vec<PathBuf>,
}

impl SourceRoots {
    /// `assets` first, then `overlays` earliest first. Overlay directories are used as given.
    #[must_use]
    pub fn new(assets: PathBuf, overlays: Vec<PathBuf>) -> Self {
        Self { assets, overlays }
    }

    /// The project's `assets/` plus the overlay roots [`OverlayRoots::load`] finds for `project`
    /// (configured directories that do not exist are left out).
    ///
    /// # Errors
    /// As [`OverlayRoots::load`].
    pub fn for_project(project: &Path) -> Result<Self, String> {
        let overlays = OverlayRoots::load(project)?.paths;
        Ok(Self::new(project.join("assets"), overlays))
    }

    /// The project's `assets/` directory.
    #[must_use]
    pub fn assets(&self) -> &Path {
        &self.assets
    }

    /// The overlay roots searched after `assets/`, earliest first.
    #[must_use]
    pub fn overlays(&self) -> &[PathBuf] {
        &self.overlays
    }

    /// Finds the source at `rel` (relative to `assets/`, `/` or `\` separated).
    ///
    /// Anything at `assets/<rel>` (a file, a directory or a symlink) wins, the same rule the
    /// cook's fill uses, and is returned as `assets` joined with `rel` as given. Otherwise the
    /// earliest overlay root holding a file at the cleaned path supplies it, provided the path is
    /// a plain relative path (see `regia_core::project_path::check_relative`) and the file,
    /// symlinks resolved, lies inside that root. `None` when no root holds it.
    #[must_use]
    pub fn resolve(&self, rel: &str) -> Option<ResolvedSource> {
        let project = self.assets.join(rel);
        if fs::symlink_metadata(&project).is_ok() {
            return Some(ResolvedSource {
                path: project,
                overlay_index: None,
            });
        }
        if self.overlays.is_empty() {
            return None;
        }
        let key = regia_core::project_path::check_relative(rel).ok()?;
        self.overlays.iter().enumerate().find_map(|(index, root)| {
            let candidate = root.join(&key);
            if !fs::metadata(&candidate).is_ok_and(|meta| meta.is_file()) {
                return None;
            }
            regia_core::project_path::check_under_root(root, &candidate).ok()?;
            Some(ResolvedSource {
                path: candidate,
                overlay_index: Some(u32::try_from(index).unwrap_or(u32::MAX)),
            })
        })
    }

    /// [`Self::resolve`]'s path, or `assets/<rel>` when no root holds `rel` (so a caller's
    /// "not found" error names the project path, as before overlay roots).
    #[must_use]
    pub fn path_for(&self, rel: &str) -> PathBuf {
        self.resolve(rel)
            .map_or_else(|| self.assets.join(rel), |found| found.path)
    }

    /// When `path` lies lexically under an overlay root, that root's index and the `/` separated
    /// path relative to it (the key the project's `assets/` would hold). A path under `assets/`
    /// itself, or under no overlay root, is `None`. The earliest matching root wins when roots
    /// nest. Used by watchers to map an overlay change to its project path.
    #[must_use]
    pub fn overlay_key(&self, path: &Path) -> Option<(usize, String)> {
        if path.starts_with(&self.assets) {
            return None;
        }
        self.overlays.iter().enumerate().find_map(|(index, root)| {
            let relative = path.strip_prefix(root).ok()?;
            let mut key = String::new();
            for component in relative.components() {
                let std::path::Component::Normal(segment) = component else {
                    return None;
                };
                if !key.is_empty() {
                    key.push('/');
                }
                key.push_str(segment.to_str()?);
            }
            (!key.is_empty()).then_some((index, key))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        regia_test_utils::write_atomic(path, bytes).expect("write");
    }

    fn rig(tag: &str) -> (regia_test_utils::TempDir, SourceRoots) {
        let dir = regia_test_utils::TempDir::new(tag);
        let roots = SourceRoots::new(
            dir.join("assets"),
            vec![dir.join("layers/overlay0"), dir.join("layers/overlay1")],
        );
        fs::create_dir_all(dir.join("assets")).expect("assets");
        fs::create_dir_all(dir.join("layers/overlay0")).expect("overlay0");
        fs::create_dir_all(dir.join("layers/overlay1")).expect("overlay1");
        (dir, roots)
    }

    #[test]
    fn a_file_only_in_an_overlay_root_resolves_there() {
        let (dir, roots) = rig("overlay-runtime-only");
        put(&dir.join("layers/overlay1/tables/w.table.ron"), b"overlay");
        let found = roots.resolve("tables/w.table.ron").expect("found");
        assert_eq!(found.path, dir.join("layers/overlay1/tables/w.table.ron"));
        assert_eq!(found.overlay_index, Some(1));
        assert_eq!(roots.path_for("tables/w.table.ron"), found.path);
    }

    #[test]
    fn the_project_wins_then_the_earliest_overlay() {
        let (dir, roots) = rig("overlay-runtime-order");
        put(&dir.join("assets/a.ron"), b"project");
        put(&dir.join("layers/overlay0/a.ron"), b"o0");
        put(&dir.join("layers/overlay0/b.ron"), b"o0");
        put(&dir.join("layers/overlay1/b.ron"), b"o1");
        let a = roots.resolve("a.ron").expect("a");
        assert_eq!((a.path, a.overlay_index), (dir.join("assets/a.ron"), None));
        let b = roots.resolve("b.ron").expect("b");
        assert_eq!(
            (b.path, b.overlay_index),
            (dir.join("layers/overlay0/b.ron"), Some(0))
        );
    }

    #[test]
    fn a_directory_in_the_project_shadows_an_overlay_file() {
        let (dir, roots) = rig("overlay-runtime-dir");
        fs::create_dir_all(dir.join("assets/x.ron")).expect("dir node");
        put(&dir.join("layers/overlay0/x.ron"), b"o0");
        let found = roots.resolve("x.ron").expect("found");
        assert_eq!(found.overlay_index, None);
    }

    #[test]
    fn escaping_paths_are_not_served_from_an_overlay() {
        let (dir, roots) = rig("overlay-runtime-escape");
        put(&dir.join("layers/secret.ron"), b"outside");
        put(&dir.join("layers/overlay0/sub/in.ron"), b"in");
        for rel in [
            "../secret.ron",
            "/secret.ron",
            "c:secret.ron",
            "sub/../../secret.ron",
        ] {
            assert_eq!(roots.resolve(rel), None, "{rel}");
        }
        let back = roots.resolve("sub\\in.ron").expect("backslash");
        assert_eq!(back.overlay_index, Some(0));
        assert_eq!(roots.path_for("missing.ron"), dir.join("assets/missing.ron"));
    }

    #[cfg(unix)]
    #[test]
    fn an_outbound_symlink_in_an_overlay_is_refused() {
        let (dir, roots) = rig("overlay-runtime-link");
        put(&dir.join("layers/outside.ron"), b"outside");
        put(&dir.join("layers/overlay1/inside.ron"), b"inside");
        std::os::unix::fs::symlink(
            dir.join("layers/outside.ron"),
            dir.join("layers/overlay0/out.ron"),
        )
        .expect("link");
        std::os::unix::fs::symlink(
            dir.join("layers/overlay1/inside.ron"),
            dir.join("layers/overlay1/in.ron"),
        )
        .expect("link");
        assert_eq!(roots.resolve("out.ron"), None);
        assert_eq!(roots.resolve("in.ron").expect("in").overlay_index, Some(1));
    }

    #[test]
    fn no_overlays_configured_searches_nothing_else() {
        let dir = regia_test_utils::TempDir::new("overlay-runtime-off");
        put(&dir.join("Regia.toml"), b"[project]\nname = \"x\"\n");
        let roots = SourceRoots::for_project(&dir).expect("roots");
        assert!(roots.overlays().is_empty());
        assert_eq!(roots.resolve("a.ron"), None);
    }

    #[test]
    fn for_project_reads_the_table_in_order() {
        let dir = regia_test_utils::TempDir::new("overlay-runtime-table");
        put(
            &dir.join("Regia.toml"),
            b"[cook.overlay_roots]\npaths = [\"gone\", \"layers/base\", \"layers/hd\"]\n",
        );
        fs::create_dir_all(dir.join("layers/base")).expect("base");
        fs::create_dir_all(dir.join("layers/hd")).expect("hd");
        let manifest = fs::read_to_string(dir.join("Regia.toml")).expect("read");
        let resolved = OverlayRoots::resolve(&dir, Some(&manifest), None).expect("resolve");
        assert_eq!(
            resolved.paths,
            vec![dir.join("layers/base"), dir.join("layers/hd")]
        );
        assert_eq!(resolved.missing, vec!["gone".to_string()]);
        let bad = OverlayRoots::resolve(&dir, Some("[cook.overlay_roots]\npaths = 3\n"), None);
        assert!(bad.is_err());
    }

    #[test]
    fn overlay_key_maps_an_overlay_path_to_its_project_key() {
        let (dir, roots) = rig("overlay-runtime-key");
        assert_eq!(
            roots.overlay_key(&dir.join("layers/overlay1/props/crate.bin")),
            Some((1, "props/crate.bin".to_string()))
        );
        assert_eq!(roots.overlay_key(&dir.join("assets/props/crate.bin")), None);
        assert_eq!(roots.overlay_key(&dir.join("elsewhere/crate.bin")), None);
        assert_eq!(roots.overlay_key(&dir.join("layers/overlay0")), None);
    }
}
