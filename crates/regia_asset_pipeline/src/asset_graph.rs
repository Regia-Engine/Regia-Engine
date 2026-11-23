// SPDX-License-Identifier: MPL-2.0
//! The asset set the validators run on, and its dependency graph (ADR-1568).
//!
//! Everything here is pure data: an [`AssetSet`] is a list of [`AssetEntry`] rows (project-relative
//! path, kind, optional id, the paths it references, its cooked size and the facts the budgets need),
//! and [`DependencyGraph`] is built from it with plain ordered maps. No file is read, so the same set
//! gives the same graph, reverse lookups and size roll-up on every run. A set round-trips through RON
//! (with `format_version`) so tools and tests can hand one in without touching the disk.
//!
//! The reachability and roll-up walks are ordinary graph traversals (breadth-first search over a
//! visited set, e.g. T. H. Cormen et al., *Introduction to Algorithms*, ch. "Elementary Graph
//! Algorithms"); cycles are legal input and are walked once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The `format_version` written into every serialized [`AssetSet`].
/// Version 2 added `AssetEntry::tags` (spec 32 § 4.2, ADR-1668); version 1 files still load.
pub const ASSET_SET_FORMAT_VERSION: u32 = 2;

/// What an asset is, as far as the validators care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    /// A mesh (carries a triangle count).
    #[serde(alias = "Mesh")]
    Mesh,
    /// A texture (carries a pixel size).
    #[serde(alias = "Texture")]
    Texture,
    /// A material.
    #[serde(alias = "Material")]
    Material,
    /// A scene; scenes are the default roots of the orphan check.
    #[serde(alias = "Scene")]
    Scene,
    /// Audio.
    #[serde(alias = "Audio")]
    Audio,
    /// Anything else.
    #[default]
    #[serde(alias = "Other")]
    Other,
}

impl AssetKind {
    /// Lowercase label used in reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Mesh => "mesh",
            Self::Texture => "texture",
            Self::Material => "material",
            Self::Scene => "scene",
            Self::Audio => "audio",
            Self::Other => "other",
        }
    }
}

/// One asset in an [`AssetSet`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AssetEntry {
    /// Project-relative path with `/` separators; unique within a set.
    pub path: String,
    /// What the asset is.
    pub kind: AssetKind,
    /// Stable asset id (for example a UUID string), when the asset has one.
    pub id: Option<String>,
    /// Paths of the assets this one references.
    pub references: Vec<String>,
    /// Size of the cooked asset in bytes.
    pub cooked_bytes: u64,
    /// Pixel size `(width, height)` of a texture.
    pub texture_size: Option<[u32; 2]>,
    /// Triangle count of a mesh.
    pub triangles: Option<u64>,
    /// Registry tags (`key` to `value`), used by the graph and size-map filters and groupings.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl AssetEntry {
    /// An entry with the given path and kind and nothing else set.
    #[must_use]
    pub fn new(path: impl Into<String>, kind: AssetKind) -> Self {
        Self {
            path: path.into(),
            kind,
            ..Self::default()
        }
    }

    /// Set the asset id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the referenced paths.
    #[must_use]
    pub fn with_references<I, S>(mut self, references: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.references = references.into_iter().map(Into::into).collect();
        self
    }

    /// Set the cooked size in bytes.
    #[must_use]
    pub fn with_cooked_bytes(mut self, bytes: u64) -> Self {
        self.cooked_bytes = bytes;
        self
    }

    /// Set the texture size.
    #[must_use]
    pub fn with_texture_size(mut self, width: u32, height: u32) -> Self {
        self.texture_size = Some([width, height]);
        self
    }

    /// Set the mesh triangle count.
    #[must_use]
    pub fn with_triangles(mut self, triangles: u64) -> Self {
        self.triangles = Some(triangles);
        self
    }

    /// Set one tag.
    #[must_use]
    pub fn with_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.insert(key.into(), value.into());
        self
    }
}

/// Errors reading or writing an [`AssetSet`].
#[derive(Debug, Error)]
pub enum AssetSetError {
    /// The RON text did not parse.
    #[error("asset set RON is invalid: {0}")]
    Parse(String),
    /// The set was written by a newer format.
    #[error("asset set format_version {found} is newer than the supported {supported}")]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// Two entries share a path.
    #[error("asset path `{0}` appears twice in the set")]
    DuplicatePath(String),
    /// Serializing failed.
    #[error("asset set could not be written: {0}")]
    Write(String),
}

impl AssetSetError {
    /// Stable lowercase code for the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "asset_set_invalid",
            Self::UnsupportedVersion { .. } => "asset_set_version",
            Self::DuplicatePath(_) => "asset_set_duplicate_path",
            Self::Write(_) => "asset_set_write",
        }
    }
}

/// A set of assets plus the roots of the orphan check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssetSet {
    /// Format version of the serialized form ([`ASSET_SET_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Paths that count as entry points. Empty: every `Scene` asset is a root.
    pub roots: Vec<String>,
    /// The assets.
    pub assets: Vec<AssetEntry>,
}

impl Default for AssetSet {
    fn default() -> Self {
        Self {
            format_version: ASSET_SET_FORMAT_VERSION,
            roots: Vec::new(),
            assets: Vec::new(),
        }
    }
}

impl AssetSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an entry.
    pub fn push(&mut self, entry: AssetEntry) {
        self.assets.push(entry);
    }

    /// The entry at `path`, if present.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&AssetEntry> {
        self.assets.iter().find(|a| a.path == path)
    }

    /// The effective roots: `roots` when set, else every scene asset, plus every top-level UI layout
    /// (`ui_layout` tag `root`, ADR-3148), sorted and deduplicated.
    #[must_use]
    pub fn effective_roots(&self) -> Vec<String> {
        let mut set: BTreeSet<String> = if self.roots.is_empty() {
            self.assets
                .iter()
                .filter(|a| a.kind == AssetKind::Scene)
                .map(|a| a.path.clone())
                .collect()
        } else {
            self.roots.iter().cloned().collect()
        };
        // Top-level UI layouts load at start, so they are roots too (ADR-3148).
        set.extend(
            self.assets
                .iter()
                .filter(|a| {
                    a.tags
                        .get(crate::registry_bridge::UI_LAYOUT_TAG)
                        .map(String::as_str)
                        == Some("root")
                })
                .map(|a| a.path.clone()),
        );
        set.into_iter().collect()
    }

    /// Parse a set from RON; rejects newer versions and duplicate paths. An older version is upgraded
    /// in memory: the returned set carries [`ASSET_SET_FORMAT_VERSION`].
    ///
    /// # Errors
    ///
    /// [`AssetSetError`] for bad RON, a newer `format_version` or a repeated path.
    pub fn from_ron(text: &str) -> Result<Self, AssetSetError> {
        let mut set: Self = ron::from_str(text).map_err(|e| AssetSetError::Parse(e.to_string()))?;
        if set.format_version > ASSET_SET_FORMAT_VERSION {
            return Err(AssetSetError::UnsupportedVersion {
                found: set.format_version,
                supported: ASSET_SET_FORMAT_VERSION,
            });
        }
        let mut seen = BTreeSet::new();
        for asset in &set.assets {
            if !seen.insert(asset.path.as_str()) {
                return Err(AssetSetError::DuplicatePath(asset.path.clone()));
            }
        }
        // An older version is upgraded in memory (v1 -> v2 only added the defaulted `tags`), so a
        // set written back records the version its fields belong to (ADR-5632).
        set.format_version = ASSET_SET_FORMAT_VERSION;
        Ok(set)
    }

    /// Serialize the set as pretty RON.
    ///
    /// # Errors
    ///
    /// [`AssetSetError::Write`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, AssetSetError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| AssetSetError::Write(e.to_string()))
    }
}

/// Own and rolled-up cooked size of one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeRollup {
    /// The asset's own cooked bytes.
    pub own_bytes: u64,
    /// Own bytes plus those of every asset it reaches, each counted once.
    pub total_bytes: u64,
    /// How many distinct assets it reaches (not counting itself).
    pub dependency_count: usize,
}

/// The asset -> references graph of an [`AssetSet`], with reverse lookup.
///
/// Only edges whose target is an asset in the set are kept; references to missing paths are listed by
/// [`DependencyGraph::missing_references`]. A self reference is dropped from the walks.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    forward: BTreeMap<String, BTreeSet<String>>,
    reverse: BTreeMap<String, BTreeSet<String>>,
    missing: BTreeMap<String, BTreeSet<String>>,
    own_bytes: BTreeMap<String, u64>,
}

impl DependencyGraph {
    /// Build the graph of `set`.
    #[must_use]
    pub fn build(set: &AssetSet) -> Self {
        let _span = tracing::debug_span!("asset_graph_build", assets = set.assets.len()).entered();
        let mut graph = Self::default();
        for asset in &set.assets {
            graph.forward.entry(asset.path.clone()).or_default();
            graph.reverse.entry(asset.path.clone()).or_default();
            graph.own_bytes.insert(asset.path.clone(), asset.cooked_bytes);
        }
        for asset in &set.assets {
            for target in &asset.references {
                if target == &asset.path {
                    continue;
                }
                if graph.forward.contains_key(target) {
                    graph
                        .forward
                        .entry(asset.path.clone())
                        .or_default()
                        .insert(target.clone());
                    graph
                        .reverse
                        .entry(target.clone())
                        .or_default()
                        .insert(asset.path.clone());
                } else {
                    graph
                        .missing
                        .entry(asset.path.clone())
                        .or_default()
                        .insert(target.clone());
                }
            }
        }
        graph
    }

    /// Whether `path` is a node.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.forward.contains_key(path)
    }

    /// Number of assets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// Whether the graph has no assets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Cooked bytes of `path`, `None` for an unknown path.
    #[must_use]
    pub fn own_bytes(&self, path: &str) -> Option<u64> {
        self.own_bytes.get(path).copied()
    }

    /// Every asset path in the graph, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.forward.keys().map(String::as_str)
    }

    /// Assets `path` references directly (sorted); empty for an unknown path.
    #[must_use]
    pub fn dependencies(&self, path: &str) -> Vec<String> {
        collect(self.forward.get(path))
    }

    /// Assets that reference `path` directly (sorted); empty for an unknown path.
    #[must_use]
    pub fn referrers(&self, path: &str) -> Vec<String> {
        collect(self.reverse.get(path))
    }

    /// Referenced paths of `path` that are not in the set (sorted).
    #[must_use]
    pub fn missing_references(&self, path: &str) -> Vec<String> {
        collect(self.missing.get(path))
    }

    /// Every asset `path` reaches through references, excluding itself (sorted).
    #[must_use]
    pub fn transitive_dependencies(&self, path: &str) -> Vec<String> {
        let mut reached = self.reach(std::iter::once(path), &self.forward);
        reached.remove(path);
        reached.into_iter().collect()
    }

    /// Every asset that reaches `path` through references, excluding itself (sorted).
    #[must_use]
    pub fn transitive_referrers(&self, path: &str) -> Vec<String> {
        let mut reached = self.reach(std::iter::once(path), &self.reverse);
        reached.remove(path);
        reached.into_iter().collect()
    }

    /// The assets reachable from `roots` (the roots included when they are in the set).
    #[must_use]
    pub fn reachable_from<'a>(&self, roots: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
        self.reach(roots, &self.forward)
    }

    /// Whether `path` takes part in a reference cycle of two or more assets (a self reference does not count).
    #[must_use]
    pub fn in_cycle(&self, path: &str) -> bool {
        let Some(direct) = self.forward.get(path) else {
            return false;
        };
        let reached = self.reach(direct.iter().map(String::as_str), &self.forward);
        reached.contains(path)
    }

    /// Own and rolled-up cooked size per asset, in path order.
    #[must_use]
    pub fn size_rollup(&self) -> BTreeMap<String, SizeRollup> {
        let _span = tracing::debug_span!("asset_size_rollup", assets = self.len()).entered();
        self.own_bytes
            .keys()
            .filter_map(|path| Some((path.clone(), self.size_of(path)?)))
            .collect()
    }

    /// The [`SizeRollup`] of one asset (`None` when it is not in the set): one walk of its
    /// dependencies, where [`Self::size_rollup`] walks from every asset. A one-asset query
    /// (`editor.asset_graph { asset }`) no longer pays for the whole set (audit-ext-b High).
    #[must_use]
    pub fn size_of(&self, path: &str) -> Option<SizeRollup> {
        let own = *self.own_bytes.get(path)?;
        let deps = self.transitive_dependencies(path);
        let total = deps
            .iter()
            .filter_map(|d| self.own_bytes.get(d))
            .fold(own, |acc, b| acc.saturating_add(*b));
        Some(SizeRollup {
            own_bytes: own,
            total_bytes: total,
            dependency_count: deps.len(),
        })
    }

    fn reach<'a>(
        &self,
        starts: impl IntoIterator<Item = &'a str>,
        edges: &BTreeMap<String, BTreeSet<String>>,
    ) -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for start in starts {
            if self.forward.contains_key(start) && seen.insert(start.to_owned()) {
                queue.push_back(start.to_owned());
            }
        }
        while let Some(node) = queue.pop_front() {
            if let Some(next) = edges.get(&node) {
                for n in next {
                    if seen.insert(n.clone()) {
                        queue.push_back(n.clone());
                    }
                }
            }
        }
        seen
    }
}

fn collect(set: Option<&BTreeSet<String>>) -> Vec<String> {
    set.map(|s| s.iter().cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AssetSet {
        let mut set = AssetSet::new();
        set.push(
            AssetEntry::new("scenes/main.scene", AssetKind::Scene)
                .with_references(["meshes/rock.mesh", "audio/wind.wav"])
                .with_cooked_bytes(10),
        );
        set.push(
            AssetEntry::new("meshes/rock.mesh", AssetKind::Mesh)
                .with_references(["materials/rock.mat"])
                .with_cooked_bytes(100),
        );
        set.push(
            AssetEntry::new("materials/rock.mat", AssetKind::Material)
                .with_references(["textures/rock.png", "textures/missing.png"])
                .with_cooked_bytes(5),
        );
        set.push(AssetEntry::new("textures/rock.png", AssetKind::Texture).with_cooked_bytes(1000));
        set.push(AssetEntry::new("audio/wind.wav", AssetKind::Audio).with_cooked_bytes(50));
        set.push(AssetEntry::new("textures/unused.png", AssetKind::Texture).with_cooked_bytes(7));
        set
    }

    #[test]
    fn forward_and_reverse_edges() {
        let g = DependencyGraph::build(&sample());
        assert_eq!(g.len(), 6);
        assert_eq!(g.dependencies("meshes/rock.mesh"), vec!["materials/rock.mat"]);
        assert_eq!(g.referrers("textures/rock.png"), vec!["materials/rock.mat"]);
        assert!(g.referrers("textures/unused.png").is_empty());
        assert!(g.dependencies("nope").is_empty());
    }

    #[test]
    fn missing_references_are_listed_not_edges() {
        let g = DependencyGraph::build(&sample());
        assert_eq!(
            g.missing_references("materials/rock.mat"),
            vec!["textures/missing.png"]
        );
        assert!(!g.contains("textures/missing.png"));
        assert_eq!(g.dependencies("materials/rock.mat"), vec!["textures/rock.png"]);
    }

    #[test]
    fn transitive_walks() {
        let g = DependencyGraph::build(&sample());
        assert_eq!(
            g.transitive_dependencies("scenes/main.scene"),
            vec![
                "audio/wind.wav",
                "materials/rock.mat",
                "meshes/rock.mesh",
                "textures/rock.png"
            ]
        );
        assert_eq!(
            g.transitive_referrers("textures/rock.png"),
            vec!["materials/rock.mat", "meshes/rock.mesh", "scenes/main.scene"]
        );
        assert!(g.transitive_dependencies("unknown").is_empty());
    }

    #[test]
    fn reachable_from_roots() {
        let g = DependencyGraph::build(&sample());
        let r = g.reachable_from(["scenes/main.scene"]);
        assert!(r.contains("textures/rock.png"));
        assert!(r.contains("scenes/main.scene"));
        assert!(!r.contains("textures/unused.png"));
        assert!(g.reachable_from(["not/there"]).is_empty());
    }

    #[test]
    fn size_rollup_counts_shared_dependencies_once() {
        let mut set = sample();
        // A second mesh sharing the material: the scene still counts the material once.
        set.push(
            AssetEntry::new("meshes/boulder.mesh", AssetKind::Mesh)
                .with_references(["materials/rock.mat"])
                .with_cooked_bytes(200),
        );
        set.assets[0].references.push("meshes/boulder.mesh".into());
        let roll = DependencyGraph::build(&set).size_rollup();
        let scene = roll["scenes/main.scene"];
        assert_eq!(scene.own_bytes, 10);
        // 10 + rock.mesh 100 + boulder 200 + wind 50 + material 5 + texture 1000
        assert_eq!(scene.total_bytes, 1365);
        assert_eq!(scene.dependency_count, 5);
        assert_eq!(roll["textures/rock.png"].total_bytes, 1000);
        assert_eq!(roll["textures/rock.png"].dependency_count, 0);
        // The one-asset form gives the same rows; an unknown path has none.
        let graph = DependencyGraph::build(&set);
        for (path, row) in &roll {
            assert_eq!(graph.size_of(path), Some(*row), "{path}");
        }
        assert_eq!(graph.size_of("no/such.asset"), None);
    }

    #[test]
    fn cycles_are_walked_once() {
        let mut set = AssetSet::new();
        set.push(
            AssetEntry::new("a", AssetKind::Material)
                .with_references(["b"])
                .with_cooked_bytes(1),
        );
        set.push(
            AssetEntry::new("b", AssetKind::Material)
                .with_references(["a"])
                .with_cooked_bytes(2),
        );
        set.push(
            AssetEntry::new("c", AssetKind::Material)
                .with_references(["c"])
                .with_cooked_bytes(4),
        );
        let g = DependencyGraph::build(&set);
        assert!(g.in_cycle("a") && g.in_cycle("b"));
        assert!(!g.in_cycle("c"), "a self reference is dropped");
        assert_eq!(g.transitive_dependencies("a"), vec!["b"]);
        let roll = g.size_rollup();
        assert_eq!(roll["a"].total_bytes, 3);
        assert_eq!(roll["c"].total_bytes, 4);
    }

    #[test]
    fn rollup_saturates() {
        let mut set = AssetSet::new();
        set.push(
            AssetEntry::new("a", AssetKind::Other)
                .with_references(["b"])
                .with_cooked_bytes(u64::MAX),
        );
        set.push(AssetEntry::new("b", AssetKind::Other).with_cooked_bytes(10));
        assert_eq!(
            DependencyGraph::build(&set).size_rollup()["a"].total_bytes,
            u64::MAX
        );
    }

    #[test]
    fn empty_graph() {
        let g = DependencyGraph::build(&AssetSet::new());
        assert!(g.is_empty());
        assert!(g.size_rollup().is_empty());
    }

    #[test]
    fn effective_roots_default_to_scenes() {
        let set = sample();
        assert_eq!(set.effective_roots(), vec!["scenes/main.scene"]);
        let mut explicit = sample();
        explicit.roots = vec!["audio/wind.wav".into()];
        assert_eq!(explicit.effective_roots(), vec!["audio/wind.wav"]);
    }

    #[test]
    fn ron_round_trip_and_defaults() {
        let set = sample();
        let text = set.to_ron().expect("serializes");
        assert!(text.contains("format_version"));
        assert_eq!(AssetSet::from_ron(&text).expect("parses"), set);
        let minimal = AssetSet::from_ron("(assets: [(path: \"a.png\", kind: texture)])").expect("defaults");
        assert_eq!(minimal.format_version, ASSET_SET_FORMAT_VERSION);
        assert_eq!(minimal.assets[0].cooked_bytes, 0);
    }

    #[test]
    fn ron_accepts_implicit_some_for_ids_and_sizes() {
        let text = "#![enable(implicit_some)]\n(assets: [(path: \"a.png\", kind: texture, id: \"t1\", texture_size: (64, 32), triangles: 3)])";
        let set = AssetSet::from_ron(text).expect("parses");
        assert_eq!(set.assets[0].id.as_deref(), Some("t1"));
        assert_eq!(set.assets[0].texture_size, Some([64, 32]));
        assert_eq!(set.assets[0].triangles, Some(3));
    }

    #[test]
    fn ron_errors_have_codes() {
        let bad = AssetSet::from_ron("(assets: [").expect_err("bad ron");
        assert_eq!(bad.code(), "asset_set_invalid");
        let newer = AssetSet::from_ron("(format_version: 99)").expect_err("newer");
        assert_eq!(newer.code(), "asset_set_version");
        let dup = AssetSet::from_ron("(assets: [(path: \"a\"), (path: \"a\")])").expect_err("dup");
        assert_eq!(dup.code(), "asset_set_duplicate_path");
    }
}
