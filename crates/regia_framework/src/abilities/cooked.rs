// SPDX-License-Identifier: MPL-2.0
//! The cooked (pak) form of `.attrs.ron`, `.effect.ron` and `.ability.ron` (spec 53 § 4.1,
//! format `abilities-cooked`, ADR-7216): a little-endian `u32` version, then postcard of a
//! [`CookedAbilityAsset`].
//!
//! A cook parses and validates the text exactly as [`AbilityAssets::load_text`] does and refuses
//! the file on any error; a load decodes the bytes and runs the same validation again, so damaged or
//! hand-edited bytes get the codes the text path would report and a cooked asset holds the same
//! values as its source. Tags are carried by name (their serde form) and level-curve references by
//! table and curve name, resolved at sample time exactly as for the text form (ADR-7216).
//!
//! A cache, not a source format: a shape change bumps [`ABILITIES_COOKED_VERSION`] and paks are
//! cooked again; there is no migration.

use serde::{Deserialize, Serialize};

use super::assets::{AbilityAssets, AssetKind, AttributesAsset, Diagnostic, has_errors, parse_asset};
use super::effects::EffectAsset;
use super::grants::AbilityAsset;
use super::modifiers::RunStateTypes;
use crate::tags::TagRegistry;

/// Current version of the cooked form (`abilities-cooked`).
pub const ABILITIES_COOKED_VERSION: u32 = 1;

/// One cooked asset: which format it came from and its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CookedAbilityAsset {
    /// From `.attrs.ron`.
    Attributes(AttributesAsset),
    /// From `.effect.ron`.
    Effect(EffectAsset),
    /// From `.ability.ron`.
    Ability(AbilityAsset),
}

impl CookedAbilityAsset {
    /// The source format.
    #[must_use]
    pub fn kind(&self) -> AssetKind {
        match self {
            Self::Attributes(_) => AssetKind::Attributes,
            Self::Effect(_) => AssetKind::Effect,
            Self::Ability(_) => AssetKind::Ability,
        }
    }
}

fn bad_file(message: impl Into<String>) -> Diagnostic {
    Diagnostic::error("cooked_bad_file", message)
}

/// Encodes `asset` without validating it (a cook validates first; tests build broken ones).
///
/// # Errors
///
/// `cooked_bad_file` when the encoder fails (never for an asset the text parser produced).
pub fn cooked_bytes(asset: &CookedAbilityAsset) -> Result<Vec<u8>, Diagnostic> {
    let out = ABILITIES_COOKED_VERSION.to_le_bytes().to_vec();
    postcard::to_extend(asset, out).map_err(|e| bad_file(format!("cooked ability asset: {e}")))
}

/// Decodes cooked bytes without validating them.
///
/// # Errors
///
/// `cooked_unsupported_version` (version 0 or newer than this build reads), `cooked_bad_file`
/// (damaged or truncated bytes, bytes after the asset).
pub fn from_cooked_bytes(bytes: &[u8]) -> Result<CookedAbilityAsset, Diagnostic> {
    let Some((version, body)) = bytes.split_first_chunk::<4>() else {
        return Err(bad_file("cooked ability asset is shorter than its version"));
    };
    let version = u32::from_le_bytes(*version);
    if version == 0 || version > ABILITIES_COOKED_VERSION {
        return Err(Diagnostic::error(
            "cooked_unsupported_version",
            format!(
                "cooked version {version}; this engine reads cooked ability assets up to {ABILITIES_COOKED_VERSION}"
            ),
        ));
    }
    let (asset, rest) = postcard::take_from_bytes::<CookedAbilityAsset>(body)
        .map_err(|e| bad_file(format!("cooked ability asset: {e}")))?;
    if !rest.is_empty() {
        return Err(bad_file(format!(
            "{} bytes after the cooked ability asset",
            rest.len()
        )));
    }
    Ok(asset)
}

/// The serde shape fingerprint of the cooked form (hashed by `cargo xtask check-formats`): the
/// three source shapes and the variant list of [`CookedAbilityAsset`].
#[must_use]
pub fn abilities_cooked_schema_fingerprint() -> String {
    use regia_core::schema::serde_variant_names;
    [
        serde_variant_names::<CookedAbilityAsset>().join(","),
        super::assets::attributes_schema_fingerprint(),
        super::assets::effect_schema_fingerprint(),
        super::assets::ability_schema_fingerprint(),
    ]
    .join(";")
}

impl AbilityAssets {
    /// Cooks the text of `path` (spec 53 § 4.1): parses and validates it as
    /// [`load_text`](Self::load_text) would against this registry (an ability's cost and cooldown
    /// must be loaded) and encodes it. The same text cooks to the same bytes.
    ///
    /// # Errors
    ///
    /// Every diagnostic when one is an error (the file would be refused at load), or
    /// `cooked_bad_file` when the encoder fails.
    pub fn cook_text(
        &self,
        path: &str,
        text: &str,
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Result<Vec<u8>, Vec<Diagnostic>> {
        let _span = tracing::debug_span!(target: "regia_framework", "abilities_cook").entered();
        let asset = parse_asset(path, text).map_err(|d| vec![d])?;
        let diags = self.validate_any(&asset, tags, run_types);
        if has_errors(&diags) {
            return Err(diags);
        }
        cooked_bytes(&asset).map_err(|d| vec![d])
    }

    /// Loads cooked bytes under `path` through the same validation as
    /// [`load_text`](Self::load_text); errors refuse the asset (warnings do not). A path that names
    /// a format must name the cooked asset's own (`cooked_kind_mismatch`). Returns every diagnostic.
    pub fn load_cooked(
        &mut self,
        path: &str,
        bytes: &[u8],
        tags: Option<&TagRegistry>,
        run_types: Option<&RunStateTypes>,
    ) -> Vec<Diagnostic> {
        let _span = tracing::debug_span!(target: "regia_framework", "abilities_load_cooked").entered();
        let asset = match from_cooked_bytes(bytes) {
            Ok(asset) => asset,
            Err(d) => return vec![d],
        };
        if let Some(kind) = AssetKind::of(path)
            && kind != asset.kind()
        {
            return vec![Diagnostic::error(
                "cooked_kind_mismatch",
                format!("`{path}` holds a cooked {:?} asset", asset.kind()),
            )];
        }
        self.validate_and_insert(path, asset, tags, run_types)
    }
}
