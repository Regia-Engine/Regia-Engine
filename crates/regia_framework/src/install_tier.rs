// SPDX-License-Identifier: MPL-2.0
//! Install-tier residency caps (`tier.ron` version 1, ADR-8036).
//!
//! A tier file limits how large a texture and a terrain layer may stay resident, and which
//! graphics preset is the ceiling. It does not change which textures ship. `texture_max_px`
//! of `0` means the authored size. `terrain_layer_max_px: None` means no mip is skipped.
//! The four built-in ids are settings: `minimal`, `low`, `high` and `ultra`.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "client")]
use crate::settings::GraphicsPreset;

/// Schema version this module reads and writes.
pub const TIER_VERSION: u32 = 1;

/// The built-in id `regia package --server` writes when the caller does not name one.
pub const DEFAULT_TIER_ID: &str = "low";

/// The built-in tier ids, in rising residency order.
pub const BUILTIN_TIER_IDS: [&str; 4] = ["minimal", "low", "high", "ultra"];

/// One install tier. Numbers are residency caps, not content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallTier {
    /// File schema. [`TIER_VERSION`] is the only version this module writes.
    pub version: u32,
    /// Tier id (`minimal`, `low`, `high`, `ultra`, or a project-defined name).
    pub tier: String,
    /// Longest texture edge, in pixels. `0` keeps the authored size.
    pub texture_max_px: u32,
    /// Texture residency budget, in mebibytes.
    pub texture_budget_mib: u32,
    /// Longest terrain-layer edge, in pixels. `None` skips no mip.
    pub terrain_layer_max_px: Option<u32>,
    /// Highest graphics preset this install may select (`Low`, `Medium`, `High`, `Ultra`,
    /// `Clean` or `Deck`).
    pub preset_max: String,
}

/// Why a tier file or a tier id was refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InstallTierError {
    /// The id is not one of the built-ins.
    #[error("install_tier_unknown: no built-in install tier `{0}`")]
    Unknown(String),
    /// `version` is not [`TIER_VERSION`].
    #[error("install_tier_bad_version: tier.ron version is {0}, want {TIER_VERSION}")]
    BadVersion(u32),
    /// The bytes are not a tier record.
    #[error("install_tier_parse: {0}")]
    Parse(String),
    /// The file could not be read.
    #[error("install_tier_io: {0}")]
    Io(String),
}

impl InstallTier {
    /// `minimal`: 512 px textures, 768 MiB, terrain layers at 512 px, preset ceiling Low.
    #[must_use]
    pub fn minimal() -> Self {
        Self::caps("minimal", 512, 768, Some(512), "Low")
    }

    /// `low`: 1024 px textures, 1024 MiB, terrain layers at 1024 px, preset ceiling Medium.
    #[must_use]
    pub fn low() -> Self {
        Self::caps("low", 1024, 1024, Some(1024), "Medium")
    }

    /// `high`: 2048 px textures, 2048 MiB, terrain layers at 2048 px, preset ceiling High.
    #[must_use]
    pub fn high() -> Self {
        Self::caps("high", 2048, 2048, Some(2048), "High")
    }

    /// `ultra`: authored texture size, 4096 MiB, no terrain-layer skip, preset ceiling Ultra.
    #[must_use]
    pub fn ultra() -> Self {
        Self::caps("ultra", 0, 4096, None, "Ultra")
    }

    fn caps(
        id: &str,
        texture_max_px: u32,
        texture_budget_mib: u32,
        terrain_layer_max_px: Option<u32>,
        preset_max: &str,
    ) -> Self {
        Self {
            version: TIER_VERSION,
            tier: id.to_owned(),
            texture_max_px,
            texture_budget_mib,
            terrain_layer_max_px,
            preset_max: preset_max.to_owned(),
        }
    }

    /// `texture_max_px == 0`: the authored texture size is the cap.
    #[must_use]
    pub fn texture_edge_uncapped(&self) -> bool {
        self.texture_max_px == 0
    }

    /// `terrain_layer_max_px` is set, so a cook may drop mips above that edge.
    #[must_use]
    pub fn skips_terrain_mips(&self) -> bool {
        self.terrain_layer_max_px.is_some()
    }

    /// The graphics preset named by `preset_max`, when it is one this engine applies. Client builds
    /// only: the presets are client settings (a server build reads and writes the tier record alone).
    #[cfg(feature = "client")]
    #[must_use]
    pub fn graphics_preset(&self) -> Option<GraphicsPreset> {
        match self.preset_max.as_str() {
            "Low" => Some(GraphicsPreset::Low),
            "Medium" => Some(GraphicsPreset::Medium),
            "High" => Some(GraphicsPreset::High),
            "Ultra" => Some(GraphicsPreset::Ultra),
            "Clean" => Some(GraphicsPreset::Clean),
            "Deck" => Some(GraphicsPreset::Deck),
            _ => None,
        }
    }

    /// Pretty RON, version 1.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn to_ron(&self) -> Result<String, InstallTierError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|err| InstallTierError::Parse(err.to_string()))
    }
}

/// The built-in tier named `id` (`minimal`, `low`, `high`, `ultra`).
///
/// # Errors
///
/// `id` is not a built-in.
pub fn builtin(id: &str) -> Result<InstallTier, InstallTierError> {
    match id {
        "minimal" => Ok(InstallTier::minimal()),
        "low" => Ok(InstallTier::low()),
        "high" => Ok(InstallTier::high()),
        "ultra" => Ok(InstallTier::ultra()),
        other => Err(InstallTierError::Unknown(other.to_owned())),
    }
}

/// Parse a `tier.ron` document.
///
/// # Errors
///
/// The text is not a version-1 tier record.
pub fn parse_tier(text: &str) -> Result<InstallTier, InstallTierError> {
    let tier: InstallTier = ron::from_str(text).map_err(|err| InstallTierError::Parse(err.to_string()))?;
    if tier.version != TIER_VERSION {
        return Err(InstallTierError::BadVersion(tier.version));
    }
    Ok(tier)
}

/// Read `path` as a `tier.ron` document.
///
/// # Errors
///
/// The file is missing, unreadable, or not a version-1 tier record.
pub fn read_tier(path: &Path) -> Result<InstallTier, InstallTierError> {
    let text = std::fs::read_to_string(path).map_err(|err| InstallTierError::Io(err.to_string()))?;
    parse_tier(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "client")]
    #[test]
    fn builtin_caps_round_trip_and_map_onto_presets() {
        let cases = [
            ("minimal", 512, 768, Some(512), "Low", GraphicsPreset::Low, false),
            (
                "low",
                1024,
                1024,
                Some(1024),
                "Medium",
                GraphicsPreset::Medium,
                false,
            ),
            (
                "high",
                2048,
                2048,
                Some(2048),
                "High",
                GraphicsPreset::High,
                false,
            ),
            ("ultra", 0, 4096, None, "Ultra", GraphicsPreset::Ultra, true),
        ];
        for (id, edge, budget, terrain, preset, graphics, uncapped) in cases {
            let tier = builtin(id).expect("built-in");
            assert_eq!(tier.version, 1);
            assert_eq!(tier.texture_max_px, edge);
            assert_eq!(tier.texture_budget_mib, budget);
            assert_eq!(tier.terrain_layer_max_px, terrain);
            assert_eq!(tier.preset_max, preset);
            assert_eq!(tier.graphics_preset(), Some(graphics));
            assert_eq!(tier.texture_edge_uncapped(), uncapped);
            assert_eq!(tier.skips_terrain_mips(), terrain.is_some());
            let text = tier.to_ron().expect("ron");
            let parsed = parse_tier(&text).expect("parse");
            assert_eq!(parsed, tier);
        }
        assert!(matches!(builtin("nope"), Err(InstallTierError::Unknown(_))));
    }

    #[test]
    fn a_newer_version_is_refused_and_none_means_no_terrain_skip() {
        let text = r#"(
            version: 2,
            tier: "low",
            texture_max_px: 1,
            texture_budget_mib: 1,
            terrain_layer_max_px: None,
            preset_max: "Medium",
        )"#;
        assert_eq!(parse_tier(text), Err(InstallTierError::BadVersion(2)));

        let ultra = InstallTier::ultra();
        assert!(ultra.texture_edge_uncapped());
        assert!(!ultra.skips_terrain_mips());
        assert_eq!(builtin(DEFAULT_TIER_ID).expect("default").tier, "low");
    }
}
