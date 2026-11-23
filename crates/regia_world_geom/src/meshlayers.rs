// SPDX-License-Identifier: MPL-2.0
//! `<map>.meshlayers.ron` v1: the source of a map's mesh layers and its world geometry (spec 121 §
//! 9.1, spec 26 § 2a.10; registry name `terrain-mesh-layers`).
//!
//! The document sits next to the map and is found like the holes file. It names one source per
//! layer (a passage description, a glTF file, collision triangles or a signed-distance volume), the
//! world geometry settings, the authored sim ground mode and the section rule. The cook
//! ([`crate::cook`]) turns it into the cooked `<map>.meshlayers` ([`crate::cooked`]).
//!
//! The passage description a `Passages` source names (`<map>.passages.ron`, [`crate::passages`]) is
//! part of the same format family: its root carries the same version number (ADR-8112).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::settings::{GeomSettings, SettingsError};

/// The version this engine writes and reads (1). A newer file is refused
/// ([`MeshLayersError::NewerVersion`]); there is no older version.
pub const MESH_LAYERS_FORMAT_VERSION: u32 = 1;

/// Default render chunk side, metres (spec 26 § 2a.5).
pub const DEFAULT_CHUNK_M: f32 = 64.0;

/// Default lattice cells per ground tile side (spec 121 § 3).
pub const DEFAULT_GROUND_TILE_CELLS: u16 = 64;

/// The smallest and largest `ground_tile_cells` (both powers of two).
pub const GROUND_TILE_CELLS_RANGE: (u16, u16) = (16, 256);

fn default_chunk_m() -> f32 {
    DEFAULT_CHUNK_M
}

fn default_ground_tile_cells() -> u16 {
    DEFAULT_GROUND_TILE_CELLS
}

fn default_true() -> bool {
    true
}

fn default_max_lods() -> u8 {
    4
}

fn default_throat_m() -> f32 {
    2.0
}

fn default_skirt_m() -> f32 {
    0.25
}

fn default_max_stitch_gap_m() -> f32 {
    1.0
}

/// The source of a map's mesh layers and its world geometry (spec 121 § 9.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshLayersDocument {
    /// [`MESH_LAYERS_FORMAT_VERSION`]. A newer file is refused.
    pub version: u32,
    /// Render chunk side, metres (spec 26 § 2a.5; default 64).
    #[serde(default = "default_chunk_m")]
    pub chunk_m: f32,
    /// World geometry settings (spec 121 § 5.1); omitted = the defaults.
    #[serde(default)]
    pub geom: GeomSettings,
    /// The sim ground mode (spec 121 § 3; ADR-8084): authored, cooked into the canonical section and
    /// hashed; never switched by the cook. Omitted = `Full`.
    #[serde(default)]
    pub ground: GroundMode,
    /// Lattice cells per ground tile side: a power of two from 16 to 256. Omitted = 64.
    #[serde(default = "default_ground_tile_cells")]
    pub ground_tile_cells: u16,
    /// The section rule.
    #[serde(default)]
    pub sections: SectionRule,
    /// Layers in id order.
    pub layers: Vec<MeshLayerSource>,
    /// Authored check waivers (ADR-8359). A waiver names a check and the cooked triangles it covers.
    /// Omitted = none. Old documents still parse.
    #[serde(default)]
    pub waive: Vec<crate::checks::CheckWaiver>,
    /// Authored probes for the cook checks (ADR-8359). Omitted = no probes, and a probe that is
    /// absent does not fail. Old documents still parse.
    #[serde(default)]
    pub audit: crate::checks::BakeAudit,
}

/// The sim ground mode (spec 121 § 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroundMode {
    /// Every ground tile at full lattice pitch. Over the memory budget it is a cook error that names
    /// the `Nested` mode that would fit.
    #[default]
    Full,
    /// Each ground tile at its own level `0..=max_level`, chosen by E1's seam rule.
    Nested {
        /// The coarsest level a tile may take (level `k` has pitch `s * 2^k`).
        max_level: u8,
    },
}

/// Where the cook cuts passages into sections (spec 121 § 5.6).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SectionRule {
    /// A cut where a sample is more than this off the chord, metres (default 4). A cover change and
    /// every junction always cut.
    pub section_chord_m: f32,
    /// Length of a blocker plug along its passage, metres (spec 121 § 5.7; default 6).
    pub plug_length_m: f32,
}

impl Default for SectionRule {
    fn default() -> Self {
        Self {
            section_chord_m: 4.0,
            plug_length_m: 6.0,
        }
    }
}

/// One layer of the document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshLayerSource {
    /// 1..=64 ASCII letters, digits, `_`, `-`, `.` (the named-hole id rule); unique.
    pub id: String,
    /// Where the layer's canonical triangles come from.
    pub source: LayerSource,
    /// Named holes it fills (each hole is filled by at most one layer).
    #[serde(default)]
    pub fills: Vec<String>,
    /// Pick layer of the chunk entities; default 0.
    #[serde(default)]
    pub map_layer: u8,
    /// Chunk entities cast shadows; default true.
    #[serde(default = "default_true")]
    pub cast_shadows: bool,
    /// Build engine-physics colliders from the canonical triangles, for hosts that do not lockstep;
    /// default false.
    #[serde(default)]
    pub collider: bool,
    /// Render material asset path (empty: the engine's default rock).
    #[serde(default)]
    pub material: String,
    /// Render LODs per chunk; default 4.
    #[serde(default = "default_max_lods")]
    pub max_lods: u8,
    /// Throat depth under a mouth, metres (spec 26 § 2a.4; default 2).
    #[serde(default = "default_throat_m")]
    pub throat_m: f32,
    /// Skirt under a mouth ring, metres (default 0.25).
    #[serde(default = "default_skirt_m")]
    pub skirt_m: f32,
    /// Largest gap the ring stitch closes, metres (default 1).
    #[serde(default = "default_max_stitch_gap_m")]
    pub max_stitch_gap_m: f32,
    /// Sections of this layer that can collapse (by passage id and arc range); each gets a blocker
    /// plug.
    #[serde(default)]
    pub blockers: Vec<BlockerSource>,
    /// The render look (displacement is bounded by the cook's render-vs-collision check).
    #[serde(default)]
    pub look: LayerLook,
}

/// Where a layer's canonical triangles come from (spec 121 § 9.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LayerSource {
    /// A passage description (`<map>.passages.ron`): the Stage A layout meshed by its pinned mesher.
    Passages {
        /// Path of the passages file, relative to the document.
        path: String,
    },
    /// A glTF render mesh with an optional collision node (canonical = the collision node, else LOD
    /// 0 of the render mesh).
    Gltf {
        /// Path of the glTF file.
        path: String,
        /// The render node (`None`: the whole scene).
        #[serde(default)]
        node: Option<String>,
        /// The collision node.
        #[serde(default)]
        collision_node: Option<String>,
    },
    /// Collision triangles plus render detail.
    Collision {
        /// Path of the collision triangles.
        path: String,
        /// The render detail added on top.
        #[serde(default)]
        detail: TunnelDetail,
    },
    /// A signed-distance volume, meshed at cook.
    Volume {
        /// Path of the volume.
        sdf: String,
        /// Iso level.
        iso: f32,
        /// Meshing cell, metres.
        cell_m: f32,
    },
}

/// Render detail on top of collision triangles. The same serde shape as the terrain crate's
/// `TunnelDetail` (spec 26 § 2a.9), so one file reads the same in both.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelDetail {
    /// Base displacement along vertex normals, metres.
    pub normal_displace: f32,
    /// Noise displacement amplitude, metres.
    pub noise_amplitude: f32,
    /// Noise spatial frequency (1/metres).
    pub noise_scale: f32,
    /// Deterministic noise seed.
    pub seed: u32,
}

impl Default for TunnelDetail {
    fn default() -> Self {
        Self {
            normal_displace: 0.05,
            noise_amplitude: 0.12,
            noise_scale: 0.9,
            seed: 7,
        }
    }
}

/// A collapsible run of a passage (spec 121 § 5.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockerSource {
    /// The passage id in the layer's source.
    pub passage: String,
    /// Arc length where the run starts, metres.
    pub from_s_m: f32,
    /// Arc length where the run ends, metres.
    pub to_s_m: f32,
    /// Whether the plug starts active (collapsed); default false.
    #[serde(default)]
    pub initially_active: bool,
}

/// A layer's render look.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayerLook {
    /// Render detail displacement (`None`: no displacement; the render mesh lies on the canonical triangles'
    /// planes). A `Passages` layer cooks displacement 0 (ADR-8175, ADR-8359): an absolute sum of
    /// `normal_displace` and `noise_amplitude` above 0.05 m is `render_off_collision`. A smaller
    /// non-zero displacement is still `cook_not_built` until displaced detail lands (lane W-16).
    pub detail: Option<TunnelDetail>,
    /// Box-projection UV tile, metres (default 4).
    pub uv_tile_m: f32,
    /// How many times the render cook splits each canonical face edge (`subdivision²` render triangles per face),
    /// 2..=16, default 4 (the T1 baker's `detail`; ADR-8175). Never 1: the canonical triangles never reach the draw
    /// list.
    pub subdivision: u8,
}

impl Default for LayerLook {
    fn default() -> Self {
        Self {
            detail: None,
            uv_tile_m: 4.0,
            subdivision: crate::render::DEFAULT_SUBDIVISION,
        }
    }
}

/// Why a mesh layers document was refused.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum MeshLayersError {
    /// (`meshlayers_newer_version`) written by a newer engine.
    #[error("meshlayers file version {found} is newer than this engine's {supported}")]
    NewerVersion {
        /// The file's version.
        found: u32,
        /// [`MESH_LAYERS_FORMAT_VERSION`].
        supported: u32,
    },
    /// (`meshlayers_parse`) not RON, or not this shape.
    #[error("meshlayers file does not parse: {0}")]
    Parse(String),
    /// (`meshlayers_invalid`) parses, but a value is out of range.
    #[error("meshlayers file is invalid: {0}")]
    Invalid(String),
}

impl MeshLayersError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NewerVersion { .. } => "meshlayers_newer_version",
            Self::Parse(_) => "meshlayers_parse",
            Self::Invalid(_) => "meshlayers_invalid",
        }
    }
}

impl From<SettingsError> for MeshLayersError {
    fn from(err: SettingsError) -> Self {
        Self::Invalid(err.to_string())
    }
}

/// Reads the root `version` of a RON document without reading the rest (a newer file may carry
/// fields this engine does not know).
///
/// # Errors
///
/// [`MeshLayersError::Parse`] when the text has no root `version`.
pub fn peek_version(text: &str) -> Result<u32, MeshLayersError> {
    #[derive(Deserialize)]
    struct Peek {
        version: u32,
    }
    ron::from_str::<Peek>(text)
        .map(|p| p.version)
        .map_err(|e| MeshLayersError::Parse(e.to_string()))
}

/// Checks a version against [`MESH_LAYERS_FORMAT_VERSION`].
///
/// # Errors
///
/// A newer version, or version 0.
pub fn check_version(found: u32) -> Result<(), MeshLayersError> {
    if found > MESH_LAYERS_FORMAT_VERSION {
        return Err(MeshLayersError::NewerVersion {
            found,
            supported: MESH_LAYERS_FORMAT_VERSION,
        });
    }
    if found == 0 {
        return Err(MeshLayersError::Invalid(
            "version 0; the format starts at 1".into(),
        ));
    }
    Ok(())
}

/// Whether `id` follows the named-hole id rule: 1..=64 ASCII letters, digits, `_`, `-`, `.`.
#[must_use]
pub fn valid_layer_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn finite(label: &str, v: f32) -> Result<(), MeshLayersError> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(MeshLayersError::Invalid(format!("{label} is not finite")))
    }
}

fn positive(label: &str, v: f32) -> Result<(), MeshLayersError> {
    finite(label, v)?;
    if v > 0.0 {
        Ok(())
    } else {
        Err(MeshLayersError::Invalid(format!("{label} must be above 0")))
    }
}

impl MeshLayersDocument {
    /// An empty v1 document with every default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: MESH_LAYERS_FORMAT_VERSION,
            chunk_m: DEFAULT_CHUNK_M,
            geom: GeomSettings::default(),
            ground: GroundMode::Full,
            ground_tile_cells: DEFAULT_GROUND_TILE_CELLS,
            sections: SectionRule::default(),
            layers: Vec::new(),
            waive: Vec::new(),
            audit: crate::checks::BakeAudit::default(),
        }
    }

    /// Parses and validates a document. The version is read first, so a newer file is refused as
    /// newer even when it carries fields this engine does not know.
    ///
    /// # Errors
    ///
    /// [`MeshLayersError`].
    pub fn from_ron(text: &str) -> Result<Self, MeshLayersError> {
        check_version(peek_version(text)?)?;
        let doc: Self = ron::from_str(text).map_err(|e| MeshLayersError::Parse(e.to_string()))?;
        doc.validate()?;
        Ok(doc)
    }

    /// The document as pretty RON.
    ///
    /// # Errors
    ///
    /// [`MeshLayersError::Parse`] if serialization fails (it does not for a valid document).
    pub fn to_ron(&self) -> Result<String, MeshLayersError> {
        let mut text = ron::ser::to_string_pretty(self, ron_pretty())
            .map_err(|e| MeshLayersError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Checks every value range: the version, the settings, `chunk_m`, `ground_tile_cells`, the
    /// nested `max_level`, the layer ids (valid and unique), each hole filled once, finite layer
    /// numbers, non-empty source paths and blocker runs.
    ///
    /// # Errors
    ///
    /// The first problem found, in field order.
    pub fn validate(&self) -> Result<(), MeshLayersError> {
        check_version(self.version)?;
        positive("chunk_m", self.chunk_m)?;
        self.geom.normalized()?;
        let (lo, hi) = GROUND_TILE_CELLS_RANGE;
        let g = self.ground_tile_cells;
        if !g.is_power_of_two() || g < lo || g > hi {
            return Err(MeshLayersError::Invalid(format!(
                "ground_tile_cells {g} is not a power of two from {lo} to {hi}"
            )));
        }
        if let GroundMode::Nested { max_level } = self.ground {
            let most = g.trailing_zeros();
            if u32::from(max_level) > most {
                return Err(MeshLayersError::Invalid(format!(
                    "ground Nested max_level {max_level} is coarser than a {g}-cell tile allows ({most})"
                )));
            }
        }
        positive("sections.section_chord_m", self.sections.section_chord_m)?;
        positive("sections.plug_length_m", self.sections.plug_length_m)?;
        if self.layers.len() > usize::from(u16::MAX) {
            return Err(MeshLayersError::Invalid("more than 65535 layers".into()));
        }
        let mut ids = BTreeSet::new();
        let mut holes = BTreeSet::new();
        for layer in &self.layers {
            let id = &layer.id;
            if !valid_layer_id(id) {
                return Err(MeshLayersError::Invalid(format!(
                    "layer id `{id}` must be 1-64 ASCII letters, digits, `_`, `-` or `.`"
                )));
            }
            if !ids.insert(id.as_str()) {
                return Err(MeshLayersError::Invalid(format!(
                    "layer id `{id}` is listed twice"
                )));
            }
            for hole in &layer.fills {
                if !valid_layer_id(hole) {
                    return Err(MeshLayersError::Invalid(format!(
                        "layer `{id}` fills `{hole}`, which is not a valid hole id"
                    )));
                }
                if !holes.insert(hole.as_str()) {
                    return Err(MeshLayersError::Invalid(format!(
                        "hole `{hole}` is filled by more than one layer"
                    )));
                }
            }
            for (label, v) in [
                ("throat_m", layer.throat_m),
                ("skirt_m", layer.skirt_m),
                ("max_stitch_gap_m", layer.max_stitch_gap_m),
                ("look.uv_tile_m", layer.look.uv_tile_m),
            ] {
                finite(&format!("layer `{id}` {label}"), v)?;
                if v < 0.0 {
                    return Err(MeshLayersError::Invalid(format!(
                        "layer `{id}` {label} is negative"
                    )));
                }
            }
            if let Some(detail) = &layer.look.detail {
                check_detail(id, detail)?;
            }
            if !(crate::render::MIN_SUBDIVISION..=crate::render::MAX_SUBDIVISION)
                .contains(&layer.look.subdivision)
            {
                return Err(MeshLayersError::Invalid(format!(
                    "layer `{id}` look.subdivision {} must be within {}..={}",
                    layer.look.subdivision,
                    crate::render::MIN_SUBDIVISION,
                    crate::render::MAX_SUBDIVISION
                )));
            }
            match &layer.source {
                LayerSource::Passages { path } | LayerSource::Gltf { path, .. } => non_empty(id, path)?,
                LayerSource::Collision { path, detail } => {
                    non_empty(id, path)?;
                    check_detail(id, detail)?;
                }
                LayerSource::Volume { sdf, iso, cell_m } => {
                    non_empty(id, sdf)?;
                    finite(&format!("layer `{id}` iso"), *iso)?;
                    positive(&format!("layer `{id}` cell_m"), *cell_m)?;
                }
            }
            for b in &layer.blockers {
                if b.passage.is_empty() {
                    return Err(MeshLayersError::Invalid(format!(
                        "layer `{id}` blocker has no passage"
                    )));
                }
                finite(&format!("layer `{id}` blocker from_s_m"), b.from_s_m)?;
                finite(&format!("layer `{id}` blocker to_s_m"), b.to_s_m)?;
                if b.from_s_m < 0.0 || b.to_s_m <= b.from_s_m {
                    return Err(MeshLayersError::Invalid(format!(
                        "layer `{id}` blocker on `{}` needs 0 <= from_s_m < to_s_m",
                        b.passage
                    )));
                }
            }
        }
        crate::checks::validate_authored(&self.waive, &self.audit).map_err(MeshLayersError::Invalid)?;
        Ok(())
    }

    /// The paths of every file the document names, in layer order (the cook hashes them into the
    /// input hash).
    #[must_use]
    pub fn named_files(&self) -> Vec<&str> {
        self.layers
            .iter()
            .map(|l| match &l.source {
                LayerSource::Passages { path }
                | LayerSource::Gltf { path, .. }
                | LayerSource::Collision { path, .. } => path.as_str(),
                LayerSource::Volume { sdf, .. } => sdf.as_str(),
            })
            .collect()
    }
}

impl Default for MeshLayersDocument {
    fn default() -> Self {
        Self::new()
    }
}

fn non_empty(id: &str, path: &str) -> Result<(), MeshLayersError> {
    if path.trim().is_empty() {
        Err(MeshLayersError::Invalid(format!(
            "layer `{id}` source path is empty"
        )))
    } else {
        Ok(())
    }
}

fn check_detail(id: &str, d: &TunnelDetail) -> Result<(), MeshLayersError> {
    for (label, v) in [
        ("normal_displace", d.normal_displace),
        ("noise_amplitude", d.noise_amplitude),
        ("noise_scale", d.noise_scale),
    ] {
        finite(&format!("layer `{id}` detail {label}"), v)?;
    }
    Ok(())
}

/// The pretty RON layout every Regia writer uses: `regia_core::ron_pretty()` with `\n` line ends on
/// every host (the default writes `\r\n` on Windows, which moves file bytes and their hashes). This
/// crate is on the lockstep closure and depends on no foundation crate, so it builds the same
/// config here instead of calling the helper (ADR-8421).
pub(crate) fn ron_pretty() -> ron::ser::PrettyConfig {
    let config: ron::ser::PrettyConfig = Default::default();
    config.new_line("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MeshLayersDocument {
        let mut doc = MeshLayersDocument::new();
        doc.ground = GroundMode::Nested { max_level: 2 };
        doc.layers.push(MeshLayerSource {
            id: "tubes".into(),
            source: LayerSource::Passages {
                path: "switchbacks.passages.ron".into(),
            },
            fills: vec!["north_mouth".into()],
            map_layer: 0,
            cast_shadows: true,
            collider: false,
            material: String::new(),
            max_lods: 4,
            throat_m: 2.0,
            skirt_m: 0.25,
            max_stitch_gap_m: 1.0,
            blockers: vec![BlockerSource {
                passage: "core".into(),
                from_s_m: 10.0,
                to_s_m: 16.0,
                initially_active: false,
            }],
            look: LayerLook::default(),
        });
        doc
    }

    #[test]
    fn a_document_round_trips_through_ron() {
        let doc = sample();
        let text = doc.to_ron().unwrap_or_default();
        assert_eq!(MeshLayersDocument::from_ron(&text), Ok(doc), "{text}");
    }

    #[test]
    fn omitted_fields_take_their_defaults() {
        let doc = MeshLayersDocument::from_ron("(version: 1, layers: [])");
        assert_eq!(doc, Ok(MeshLayersDocument::new()));
        let one = MeshLayersDocument::from_ron("(version: 1, geom: (max_floors: 5), layers: [])")
            .map(|d| (d.geom.max_floors, d.geom.index_cell_m));
        assert_eq!(one, Ok((5, 16.0)));
    }

    #[test]
    fn a_newer_document_is_refused_before_its_unknown_fields() {
        let err = MeshLayersDocument::from_ron("(version: 2, layers: [], a_v2_field: true)");
        assert_eq!(
            err.as_ref().map_err(MeshLayersError::code),
            Err("meshlayers_newer_version"),
            "{err:?}"
        );
    }

    #[test]
    fn bad_values_are_named() {
        for (text, needle) in [
            (
                "(version: 1, ground_tile_cells: 48, layers: [])",
                "ground_tile_cells",
            ),
            (
                "(version: 1, ground_tile_cells: 16, ground: Nested(max_level: 5), layers: [])",
                "max_level",
            ),
            ("(version: 1, chunk_m: 0.0, layers: [])", "chunk_m"),
            ("(version: 0, layers: [])", "version 0"),
            ("(version: 1, geom: (max_floors: 0), layers: [])", "max_floors"),
            (
                "(version: 1, layers: [(id: \"a b\", source: Passages(path: \"p\"))])",
                "layer id",
            ),
            (
                "(version: 1, layers: [(id: \"a\", source: Passages(path: \"p\")), (id: \"a\", source: Passages(path: \"q\"))])",
                "listed twice",
            ),
            (
                "(version: 1, layers: [(id: \"a\", source: Passages(path: \"p\"), fills: [\"h\"]), (id: \"b\", source: Passages(path: \"q\"), fills: [\"h\"])])",
                "more than one layer",
            ),
        ] {
            let err = MeshLayersDocument::from_ron(text);
            match err {
                Err(MeshLayersError::Invalid(msg)) => assert!(msg.contains(needle), "{text}: {msg}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        assert!(matches!(
            MeshLayersDocument::from_ron("(version: 1, layers: [], extra: 1)"),
            Err(MeshLayersError::Parse(_))
        ));
    }
}
