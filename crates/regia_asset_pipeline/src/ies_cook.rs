// SPDX-License-Identifier: MPL-2.0
//! Cooks an `.ies` photometric profile (IESNA LM-63, `docs/formats/ies-profile-v1.md`, ADR-2592): parses the
//! text, resamples it onto the renderer's grid and writes a [`CookedIesProfile`] payload under the type path
//! [`IES_PROFILE_TYPE_PATH`], which `regia_render::LightProfile` and the path tracer read.

use std::path::Path;

use regia_asset::ies::{CookedIesProfile, IES_PROFILE_TYPE_PATH, IesError, IesFile};

/// Importer name recorded in the asset database and the `.meta` sidecar.
pub const IES_IMPORTER: &str = "ies";

/// Parses and resamples LM-63 text (no file IO): the cook step `cook_project` runs for `.ies` sources.
///
/// # Errors
/// The parse or resample error ([`IesError`]), for text that is not UTF-8 too.
pub fn cook_ies_bytes(bytes: &[u8]) -> Result<CookedIesProfile, IesError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| IesError::Invalid(format!("not UTF-8 at byte {}", e.valid_up_to())))?;
    CookedIesProfile::from_file(&IesFile::parse(text)?)
}

/// [`cook_ies_bytes`] on a file.
///
/// # Errors
/// [`IesError::Invalid`] naming the path when the file cannot be read; otherwise as [`cook_ies_bytes`].
pub fn cook_ies(path: &Path) -> Result<CookedIesProfile, IesError> {
    let bytes = std::fs::read(path).map_err(|e| IesError::Invalid(format!("{}: {e}", path.display())))?;
    cook_ies_bytes(&bytes)
}

/// The cook step's row: importer, settings hash (the source's), id, type path, payload.
type CookedRow = (String, [u8; 32], regia_asset::AssetId, String, Vec<u8>);

/// Cooks one `.ies` source into its [`CookedRow`].
pub(crate) fn cook_ies_source(source: &Path, bytes: &[u8]) -> Result<CookedRow, crate::CookError> {
    let meta =
        crate::AssetDatabase::load_or_create_meta(source, IES_IMPORTER).map_err(crate::CookError::Db)?;
    let cooked =
        cook_ies_bytes(bytes).map_err(|e| crate::CookError::Ies(format!("{}: {e}", source.display())))?;
    Ok((
        IES_IMPORTER.to_string(),
        *blake3::hash(bytes).as_bytes(),
        meta.id,
        IES_PROFILE_TYPE_PATH.to_string(),
        cooked.to_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_cooks_and_bad_text_is_an_error() {
        let cooked = cook_ies_bytes(regia_asset::ies::example_text().as_bytes()).expect("cook");
        assert_eq!(cooked.peak_candela, 1000.0);
        assert!(cook_ies_bytes(b"TILT=NONE\n1 -1 1 1 1 1 2 0 0 0\n").is_err());
        assert!(cook_ies_bytes(&[0xff, 0xfe, 0x00]).is_err());
    }
}
