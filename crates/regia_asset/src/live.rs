// SPDX-License-Identifier: MPL-2.0
//! Live asset reload: what changed, what was reloaded, and the watcher's status
//! (`docs/specs/12-assets.md` § 8).
//!
//! These types carry no file watcher and compile on every platform. The watcher itself lives in
//! [`crate::hot_reload`] (`dev-assets` feature, desktop only); the cooker side in
//! `regia_asset_pipeline::live`. Systems that cache data derived from an asset read
//! [`AssetReloaded`] (cooked assets the [`crate::AssetServer`] replaced in place) or
//! [`SourceAssetChanged`] (source files the runtime reads directly: shaders, levels, VFX, UI
//! layouts, audio) and rebuild.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;

use crate::id::AssetId;

/// The kind of a source asset, from its file name. Decides who reacts to a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SourceKind {
    /// `.glb` / `.gltf` (cooked to a mesh plus sub-assets).
    Mesh,
    /// `.png` / `.jpg` / `.jpeg` (cooked to a texture or heightfield), `.dds` / `.tga` / `.bmp`
    /// (cooked to a texture, ADR-1855).
    Texture,
    /// `.material.ron` (cooked standard material).
    Material,
    /// `.matgraph.ron` (material graph; cooked to a `CookedMaterialGraph`).
    MaterialGraph,
    /// `.matinst.ron` (material-graph instance; cooked like a graph).
    MaterialInstance,
    /// `.matfunc.ron` (material function; cooked into the graphs that call it).
    MaterialFunction,
    /// `.wgsl` (engine or game shader).
    Shader,
    /// `.level.ron`, or a file inside a level directory (`*.level.d/`).
    Level,
    /// `.layers.ron` (data layer definitions).
    LayerDefs,
    /// `.prefab.ron` or `.variant.ron` (a prefab variant, ADR-2323).
    Prefab,
    /// `*terrain.layers.ron` (cooked through the tiled terrain path).
    TerrainLayers,
    /// `.scatter.ron` (cooked scatter rules).
    Scatter,
    /// `.decal.ron` (decal prefab).
    Decal,
    /// `.vfx.ron` (particle system).
    Vfx,
    /// `.wav` / `.ogg` / `.flac` / `.mp3`.
    Audio,
    /// `.ui.ron` (UI layout template).
    UiLayout,
    /// `.camera.ron`, `.smartobject.ron`, `.detutility.ron`, `.utility.ron`, `.chooser.ron`, `.curves.ron`,
    /// `.curves.csv`, `.table.ron`, `.table.csv`, `.formation.ron`, `.rules.ron`, `.gait.ron`: gameplay data whose
    /// owners re-read the source file when it changes (they cook as well, ADR-2030).
    GameplayData,
    /// Anything else (`.bin` buffers, `.meta` sidecars, unknown files).
    Other,
}

impl SourceKind {
    /// Classifies `path` by its file name. Case-insensitive on the extension.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Self::Other;
        };
        let name = name.to_ascii_lowercase();
        let in_level_dir = path.components().any(|part| {
            part.as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".level.d")
        });
        if in_level_dir {
            return Self::Level;
        }
        let suffixes: [(&str, Self); 26] = [
            (".material.ron", Self::Material),
            (".matgraph.ron", Self::MaterialGraph),
            (".matinst.ron", Self::MaterialInstance),
            (".matfunc.ron", Self::MaterialFunction),
            (".level.ron", Self::Level),
            (".prefab.ron", Self::Prefab),
            (".variant.ron", Self::Prefab),
            ("terrain.layers.ron", Self::TerrainLayers),
            (".layers.ron", Self::LayerDefs),
            (".scatter.ron", Self::Scatter),
            (".decal.ron", Self::Decal),
            (".vfx.ron", Self::Vfx),
            (".ui.ron", Self::UiLayout),
            (".wgsl", Self::Shader),
            (".camera.ron", Self::GameplayData),
            (".smartobject.ron", Self::GameplayData),
            // Before `.utility.ron`: the deterministic utility catalogue (ADR-8609).
            (".detutility.ron", Self::GameplayData),
            (".utility.ron", Self::GameplayData),
            (".chooser.ron", Self::GameplayData),
            (".curves.ron", Self::GameplayData),
            (".curves.csv", Self::GameplayData),
            (".table.ron", Self::GameplayData),
            (".table.csv", Self::GameplayData),
            (".formation.ron", Self::GameplayData),
            // Rule sets: a match mode as data (ADR-8694).
            (".rules.ron", Self::GameplayData),
            // Gait settings (gap G7): `regia_animation::gait_asset` reads the source on change.
            (".gait.ron", Self::GameplayData),
        ];
        for (suffix, kind) in suffixes {
            if name.ends_with(suffix) {
                return kind;
            }
        }
        match name.rsplit_once('.').map(|(_, ext)| ext) {
            Some("glb" | "gltf") => Self::Mesh,
            Some("png" | "jpg" | "jpeg" | "dds" | "tga" | "bmp" | "exr" | "hdr") => Self::Texture,
            Some("wav" | "ogg" | "flac" | "mp3") => Self::Audio,
            _ => Self::Other,
        }
    }

    /// True when the cooker turns this kind into `.fckd` blobs the [`crate::AssetServer`] loads.
    /// The other kinds are read from source by the runtime system that owns them.
    #[must_use]
    pub fn is_cooked(self) -> bool {
        matches!(
            self,
            Self::Mesh
                | Self::Texture
                | Self::Material
                | Self::MaterialGraph
                | Self::MaterialInstance
                | Self::TerrainLayers
                | Self::Scatter
        )
    }

    /// True when some runtime system parses this kind from its source file (so a change is
    /// forwarded as [`SourceAssetChanged`]): everything but meshes, textures, materials,
    /// material graphs (the runtime loads their cooked form) and unknown files. Terrain layer
    /// stacks and scatter rules are both cooked and parsed.
    #[must_use]
    pub fn is_read_at_runtime(self) -> bool {
        !matches!(
            self,
            Self::Mesh
                | Self::Texture
                | Self::Material
                | Self::MaterialGraph
                | Self::MaterialInstance
                | Self::MaterialFunction
                | Self::Other
        )
    }

    /// Short lower-case name for logs, toasts and the agent protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mesh => "mesh",
            Self::Texture => "texture",
            Self::Material => "material",
            Self::MaterialGraph => "matgraph",
            Self::MaterialInstance => "matinst",
            Self::MaterialFunction => "matfunc",
            Self::Shader => "shader",
            Self::Level => "level",
            Self::LayerDefs => "layer_defs",
            Self::Prefab => "prefab",
            Self::TerrainLayers => "terrain_layers",
            Self::Scatter => "scatter",
            Self::Decal => "decal",
            Self::Vfx => "vfx",
            Self::Audio => "audio",
            Self::UiLayout => "ui_layout",
            Self::GameplayData => "gameplay_data",
            Self::Other => "other",
        }
    }
}

/// A source file the runtime reads directly changed on disk (sent after the debounce).
///
/// `path` is absolute. Owners of the kind (render for shaders, VFX for `.vfx.ron`, …) re-read it.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct SourceAssetChanged {
    /// Absolute path of the changed file.
    pub path: PathBuf,
    /// Its kind.
    pub kind: SourceKind,
}

impl SourceAssetChanged {
    /// A change for `path`, classified by [`SourceKind::of`].
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let kind = SourceKind::of(&path);
        Self { path, kind }
    }
}

/// The [`crate::AssetServer`] replaced a loaded cooked asset in place. Handles stay valid; systems
/// that cached data derived from it (GPU uploads, colliders, pipelines) rebuild.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct AssetReloaded {
    /// The reloaded asset.
    pub id: AssetId,
    /// Which storage it lives in.
    pub kind: LoadedKind,
}

/// The storage a cooked asset was reloaded into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoadedKind {
    /// [`crate::CpuMesh`].
    Mesh,
    /// [`crate::CookedTexture`].
    Texture,
    /// [`crate::CookedImage`].
    Image,
    /// [`crate::CookedStandardMaterial`].
    Material,
    /// A payload another crate decodes ([`crate::AssetServer::track_external`]): only its
    /// [`crate::AssetServer::generation`] moved.
    External,
    /// A value of a registered [`crate::AssetLoader`] (ADR-7202); the assets that depend on it
    /// reloaded too and are reported with this kind.
    Generic,
}

/// Outcome of one reload step, for the status line and `assets.watch_status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchOutcome {
    /// Cooked (or re-read) and reloaded.
    Reloaded,
    /// The cook or the reload failed; the previous version stays live.
    Error(String),
}

/// One entry of [`AssetWatchStatus::recent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEntry {
    /// Project-relative path (`assets/...`) when known, else the absolute path.
    pub path: String,
    /// Kind of the source.
    pub kind: SourceKind,
    /// Milliseconds from the debounced change to the reload (cook included).
    pub millis: u64,
    /// What happened.
    pub outcome: WatchOutcome,
}

impl WatchEntry {
    /// The status-line text: `reloaded X in 120 ms` or `cook error in Y: …`.
    #[must_use]
    pub fn toast(&self) -> String {
        match &self.outcome {
            WatchOutcome::Reloaded => format!("reloaded {} in {} ms", self.path, self.millis),
            WatchOutcome::Error(err) => format!("cook error in {}: {err}", self.path),
        }
    }
}

/// Live-reload status: whether a watcher runs, on what, and the last results.
///
/// Inserted by [`crate::AssetPlugin`]; the editor and `regia run --dev` fill it. The agent
/// method `assets.watch_status` returns it.
#[derive(Resource, Clone, Debug, Default)]
pub struct AssetWatchStatus {
    /// A source or cooked-cache watcher is running.
    pub watching: bool,
    /// Directories being watched.
    pub roots: Vec<PathBuf>,
    /// Debounce quiet time in milliseconds.
    pub debounce_ms: u64,
    /// Total successful reloads.
    pub reloads: u64,
    /// Total failed cooks/reloads.
    pub errors: u64,
    /// The last [`AssetWatchStatus::RECENT`] entries, newest last.
    pub recent: VecDeque<WatchEntry>,
}

impl AssetWatchStatus {
    /// How many entries [`AssetWatchStatus::recent`] keeps.
    pub const RECENT: usize = 32;

    /// Records one result and returns its status line.
    pub fn record(&mut self, entry: WatchEntry) -> String {
        match entry.outcome {
            WatchOutcome::Reloaded => self.reloads += 1,
            WatchOutcome::Error(_) => self.errors += 1,
        }
        let line = entry.toast();
        if self.recent.len() == Self::RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(entry);
        line
    }

    /// The newest entry's status line.
    #[must_use]
    pub fn last_line(&self) -> Option<String> {
        self.recent.back().map(WatchEntry::toast)
    }
}

/// Extra directories a dev build's source watcher covers besides the project's `assets/`
/// (for example the engine's WGSL directory in a dev checkout). Plugins push to it in `build`;
/// [`crate::AssetPlugin`] reads it in `finish`.
#[derive(Resource, Clone, Debug, Default)]
pub struct DevWatchRoots(pub Vec<PathBuf>);

/// `assets/...` form of `path` relative to `project_root`, or the path as given.
#[must_use]
pub fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .map(crate::normalise_asset_path)
        .unwrap_or_else(|_| crate::normalise_asset_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_kind() {
        let cases = [
            ("a/b.glb", SourceKind::Mesh),
            ("a/b.GLTF", SourceKind::Mesh),
            ("t.png", SourceKind::Texture),
            ("t.JPG", SourceKind::Texture),
            ("m.material.ron", SourceKind::Material),
            ("g.matgraph.ron", SourceKind::MaterialGraph),
            ("i.matinst.ron", SourceKind::MaterialInstance),
            ("s/forward.wgsl", SourceKind::Shader),
            ("l/intro.level.ron", SourceKind::Level),
            ("l/forest.level.d/level.ron", SourceKind::Level),
            ("l/forest.level.d/entities/8f/8f1c.entity.ron", SourceKind::Level),
            ("l/forest.layers.ron", SourceKind::LayerDefs),
            ("p/crate.prefab.ron", SourceKind::Prefab),
            ("maps/bore.terrain.layers.ron", SourceKind::TerrainLayers),
            ("maps/terrain.layers.ron", SourceKind::TerrainLayers),
            ("rocks.scatter.ron", SourceKind::Scatter),
            ("scorch.decal.ron", SourceKind::Decal),
            ("camera/player.camera.ron", SourceKind::GameplayData),
            ("props/chair.SmartObject.ron", SourceKind::GameplayData),
            ("anim/hit.chooser.ron", SourceKind::GameplayData),
            ("tables/a.curves.ron", SourceKind::GameplayData),
            ("tables/b.curves.csv", SourceKind::GameplayData),
            ("tables/weapons.table.ron", SourceKind::GameplayData),
            ("tables/Weapons.TABLE.csv", SourceKind::GameplayData),
            ("ai/squad.formation.ron", SourceKind::GameplayData),
            ("rules/ctf_long.rules.ron", SourceKind::GameplayData),
            ("ai/skirmish.detutility.ron", SourceKind::GameplayData),
            ("units/walker.gait.ron", SourceKind::GameplayData),
            ("fire.vfx.ron", SourceKind::Vfx),
            ("hit.wav", SourceKind::Audio),
            ("music.ogg", SourceKind::Audio),
            ("hud.ui.ron", SourceKind::UiLayout),
            ("buf.bin", SourceKind::Other),
            ("x.png.meta", SourceKind::Other),
            ("plain.ron", SourceKind::Other),
        ];
        for (path, kind) in cases {
            assert_eq!(SourceKind::of(Path::new(path)), kind, "{path}");
        }
        assert!(SourceKind::Texture.is_cooked());
        assert!(!SourceKind::Vfx.is_cooked());
        assert!(SourceKind::Scatter.is_cooked() && SourceKind::Scatter.is_read_at_runtime());
        assert!(!SourceKind::Mesh.is_read_at_runtime() && SourceKind::Shader.is_read_at_runtime());
    }

    /// Review of the det-utility buildout (bug): `.detutility.ron` was `Other`, so the dev source
    /// watcher dropped it and logs named it `other`.
    #[test]
    fn det_utility_sources_are_gameplay_data() {
        let kind = SourceKind::of(Path::new("assets/ai/skirmish.detutility.ron"));
        assert_eq!(kind, SourceKind::GameplayData);
        assert!(kind.is_read_at_runtime());
        assert_eq!(kind.as_str(), "gameplay_data");
        assert_eq!(
            SourceKind::of(Path::new("assets/ai/Skirmish.DetUtility.RON")),
            SourceKind::GameplayData
        );
        // `.utility.ron` must not steal the longer suffix, nor lose its own.
        assert_eq!(
            SourceKind::of(Path::new("ai/brain.utility.ron")),
            SourceKind::GameplayData
        );
    }

    #[test]
    fn status_keeps_a_bounded_history_and_counts() {
        let mut status = AssetWatchStatus::default();
        for i in 0..40 {
            let outcome = if i % 10 == 0 {
                WatchOutcome::Error("bad".into())
            } else {
                WatchOutcome::Reloaded
            };
            status.record(WatchEntry {
                path: format!("assets/{i}.png"),
                kind: SourceKind::Texture,
                millis: 120,
                outcome,
            });
        }
        assert_eq!(status.recent.len(), AssetWatchStatus::RECENT);
        assert_eq!(status.reloads, 36);
        assert_eq!(status.errors, 4);
        assert_eq!(
            status.last_line().as_deref(),
            Some("reloaded assets/39.png in 120 ms")
        );
        let err = WatchEntry {
            path: "assets/m.material.ron".into(),
            kind: SourceKind::Material,
            millis: 3,
            outcome: WatchOutcome::Error("line 2: expected `)`".into()),
        };
        assert_eq!(
            err.toast(),
            "cook error in assets/m.material.ron: line 2: expected `)`"
        );
    }
}
