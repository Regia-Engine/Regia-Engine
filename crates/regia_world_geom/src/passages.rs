// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-core/examples/tunnels_export.rs, commit d6ed37677), ADR-8012.
// Lines 1-24, mark: adapt (named issue: the example wrote DO27's export to its own content tree; Regia
// keeps the passage description as `<map>.passages.ron` and writes `do27-tunnels/1` back on request).
// Plan row W-06, ADR-8112.

//! `<map>.passages.ron`: a passage description as data, the file a `Passages` layer source names
//! (spec 121 § 9.1, spec 26 § 2a.9), and its import from and export to DO27's `do27-tunnels/1` JSON.
//!
//! The file is part of the `terrain-mesh-layers` format family (ADR-8112): its root `version` is
//! [`MESH_LAYERS_FORMAT_VERSION`], and its shape is in that format's fingerprint. It holds the
//! Stage A layout (`TunnelLayout`, which keeps its own layout version) and, when it was imported, the
//! baked blocks DO27's export carried: the sampled centrelines its sim armed from, the mouth hole
//! list and the triangle count. The cook re-samples the layout on the map's terrain and fails when a
//! stored sampled block disagrees (`passages_drift`), so a stale import never cooks silently.
//!
//! `do27-tunnels/1` is a foreign host format: Regia imports it and never freezes it. The export
//! ([`PassagesDocument::to_do27_json`]) writes the bytes `TunnelLayout::export_json` writes for the
//! same layout and terrain, so DO27 can keep reading the file until it switches to the cooked
//! geometry. Adapted from DO27's `ir-core/examples/tunnels_export.rs` (d6ed37677, regia-absorb-1).

use serde::{Deserialize, Serialize};

use crate::compat::layout::{EXPORT_FORMAT, MouthHole, SampledLayout, TunnelLayout};
use crate::compat::pack::parse_tunnels;
use crate::compat::sampler::TerrainSampler;
use crate::meshlayers::{MESH_LAYERS_FORMAT_VERSION, MeshLayersError, check_version, peek_version};

/// The free-text `coordinates` field of a `do27-tunnels/1` export (the text
/// `TunnelLayout::export_json` writes; nothing reads it back).
pub const EXPORT_COORDINATES: &str =
    "sim metres: x east, y up, z north; a z-up centimetre renderer takes (x * 100, -z * 100, y * 100)";

/// A passage description (`<map>.passages.ron`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassagesDocument {
    /// [`MESH_LAYERS_FORMAT_VERSION`]; a newer file is refused.
    pub version: u32,
    /// The passages, openings, junctions and crawlways (the Stage A layout, with its own `version`).
    pub layout: TunnelLayout,
    /// The sampled centrelines of an imported file (the block DO27's sim armed from). `None` for a
    /// description authored in Regia; the cook samples the layout itself.
    #[serde(default)]
    pub sampled: Option<SampledLayout>,
    /// The mouth hole list of an imported file.
    #[serde(default)]
    pub holes: Option<Vec<MouthHole>>,
    /// The triangle count an imported file recorded.
    #[serde(default)]
    pub triangle_count: Option<u64>,
}

/// Why a passages file or a `do27-tunnels/1` file was refused.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PassagesError {
    /// The document version or shape (`meshlayers_newer_version`, `meshlayers_parse`,
    /// `meshlayers_invalid`).
    #[error(transparent)]
    Document(#[from] MeshLayersError),
    /// (`passages_import`) the `do27-tunnels/1` JSON does not parse or is not that format.
    #[error("do27-tunnels/1 import: {0}")]
    Import(String),
    /// (`passages_unsampled`) an export needs the sampled block, which a Regia-authored file lacks.
    #[error("the passages file has no sampled block; export needs one (import a do27-tunnels/1 file)")]
    Unsampled,
}

impl PassagesError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Document(e) => e.code(),
            Self::Import(_) => "passages_import",
            Self::Unsampled => "passages_unsampled",
        }
    }
}

impl PassagesDocument {
    /// A Regia-authored description: the layout only.
    #[must_use]
    pub fn from_layout(layout: TunnelLayout) -> Self {
        Self {
            version: MESH_LAYERS_FORMAT_VERSION,
            layout,
            sampled: None,
            holes: None,
            triangle_count: None,
        }
    }

    /// Imports DO27's `do27-tunnels/1` JSON (`tunnels.json`): the layout and the baked blocks,
    /// checked by the Stage A parser (`compat::pack::parse_tunnels`: format tag, non-empty map id and
    /// passages, the sampled block present).
    ///
    /// # Errors
    ///
    /// [`PassagesError::Import`].
    pub fn import_do27_json(text: &str) -> Result<Self, PassagesError> {
        let file = parse_tunnels(text).map_err(PassagesError::Import)?;
        Ok(Self {
            version: MESH_LAYERS_FORMAT_VERSION,
            layout: file.layout,
            sampled: Some(file.sampled),
            holes: file.holes,
            triangle_count: file.triangle_count,
        })
    }

    /// Imports a `do27-tunnels-v2/1` description (the input of DO27's one-description baker,
    /// [`crate::tunnel_bake`]) as a Regia-authored file: validated by
    /// [`crate::tunnel_bake::validate_description`] and converted by `Description::to_layout`. The
    /// description's looks, gameplay block and level tags are not part of a passages file and are
    /// dropped (ADR-8175); the cook samples the layout itself (no sampled block).
    ///
    /// # Errors
    ///
    /// [`PassagesError::Import`] (`passages_import`) when the text is not JSON or the description is
    /// refused (the message carries the validation message and `tunnel_description_invalid`).
    pub fn import_description(text: &str, map_id: &str) -> Result<Self, PassagesError> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| PassagesError::Import(format!("{}: not JSON: {e}", crate::tunnel_bake::FORMAT)))?;
        let desc = crate::tunnel_bake::validate_description(&value).map_err(|e| {
            PassagesError::Import(format!(
                "{}: {e} (tunnel_description_invalid)",
                crate::tunnel_bake::FORMAT
            ))
        })?;
        Ok(Self::from_layout(desc.to_layout(map_id)))
    }

    /// Whether `text` looks like a `do27-tunnels-v2/1` description (a JSON object with `passages`
    /// and no `format` key) rather than a `do27-tunnels/1` export.
    #[must_use]
    pub fn is_description(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|v| v.get("format").is_none() && v.get("passages").is_some())
    }

    /// Writes `do27-tunnels/1` JSON. For an imported file this is byte-identical to the file it came
    /// from when that file was written by `TunnelLayout::export_json` (the import round trip). A file
    /// without holes writes the hole list derived from its sampled block, as the export does.
    ///
    /// # Errors
    ///
    /// [`PassagesError::Unsampled`] when the file has no sampled block.
    pub fn to_do27_json(&self) -> Result<String, PassagesError> {
        let sampled = self.sampled.as_ref().ok_or(PassagesError::Unsampled)?;
        #[derive(Serialize)]
        struct Export<'a> {
            format: &'static str,
            coordinates: &'static str,
            layout: &'a TunnelLayout,
            sampled: &'a SampledLayout,
            holes: Vec<MouthHole>,
            triangle_count: Option<u64>,
        }
        let holes = match &self.holes {
            Some(h) => h.clone(),
            None => sampled.mouth_holes(&self.layout.openings),
        };
        let export = Export {
            format: EXPORT_FORMAT,
            coordinates: EXPORT_COORDINATES,
            layout: &self.layout,
            sampled,
            holes,
            triangle_count: self.triangle_count,
        };
        let mut text =
            serde_json::to_string_pretty(&export).map_err(|e| PassagesError::Import(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// A Regia description re-baked for DO27 on `terrain`: the layout's own `export_json`.
    #[must_use]
    pub fn export_do27_json_on(&self, terrain: Option<&TerrainSampler>) -> String {
        self.layout.export_json(terrain)
    }

    /// Parses a `<map>.passages.ron`; the version is checked first.
    ///
    /// # Errors
    ///
    /// [`PassagesError::Document`].
    pub fn from_ron(text: &str) -> Result<Self, PassagesError> {
        check_version(peek_version(text)?)?;
        let doc: Self = ron::from_str(text).map_err(|e| MeshLayersError::Parse(e.to_string()))?;
        doc.validate()?;
        Ok(doc)
    }

    /// The document as pretty RON.
    ///
    /// # Errors
    ///
    /// [`PassagesError::Document`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, PassagesError> {
        let mut text = ron::ser::to_string_pretty(self, crate::meshlayers::ron_pretty())
            .map_err(|e| MeshLayersError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Checks the version, a non-empty map id and passages, and finite coordinates and sizes.
    ///
    /// # Errors
    ///
    /// [`PassagesError::Document`] with `meshlayers_invalid`.
    pub fn validate(&self) -> Result<(), PassagesError> {
        check_version(self.version)?;
        let invalid = |m: String| PassagesError::Document(MeshLayersError::Invalid(m));
        if self.layout.map_id.is_empty() {
            return Err(invalid("passages layout.map_id is empty".into()));
        }
        if self.layout.passages.is_empty() {
            return Err(invalid("passages layout has no passages".into()));
        }
        let fin = |v: f32| v.is_finite();
        if !fin(self.layout.sample_step_m) || self.layout.sample_step_m <= 0.0 {
            return Err(invalid(
                "passages layout.sample_step_m must be finite and above 0".into(),
            ));
        }
        for p in &self.layout.passages {
            if !fin(p.width_m) || !fin(p.ceiling_m) || p.points.iter().flatten().any(|v| !fin(*v)) {
                return Err(invalid(format!("passage `{}` has a non-finite number", p.id)));
            }
        }
        for o in &self.layout.openings {
            if o.top.iter().any(|v| !fin(*v)) {
                return Err(invalid(format!("opening `{}` has a non-finite top", o.id)));
            }
        }
        for j in &self.layout.junctions {
            if j.at.iter().any(|v| !fin(*v)) {
                return Err(invalid(format!("junction `{}` has a non-finite position", j.id)));
            }
        }
        for c in &self.layout.crawlways {
            if !fin(c.width_m) || !fin(c.height_m) || c.points.iter().flatten().any(|v| !fin(*v)) {
                return Err(invalid(format!("crawlway `{}` has a non-finite number", c.id)));
            }
        }
        if let Some(s) = &self.sampled {
            for p in &s.passages {
                let bad = p
                    .samples
                    .iter()
                    .any(|s| s.p.iter().chain(s.left.iter()).any(|v| !fin(*v)) || !fin(s.s));
                if bad {
                    return Err(invalid(format!(
                        "sampled passage `{}` has a non-finite number",
                        p.id
                    )));
                }
            }
        }
        Ok(())
    }
}
