// SPDX-License-Identifier: MPL-2.0
//! Fuzz targets for IES LM-63 photometric profiles (ADR-2592): the `.ies` text the importer reads (`ies`) and the
//! cooked `FIES` payload the renderer and the path tracer load (`ies-profile`). Fixtures are the format crate's
//! own example profile.

use std::fmt::Display;

use regia_asset::ies::{CookedIesProfile, IES_PROFILE_FORMAT_VERSION, IesFile, example_text};

use super::formats::text;
use super::{FuzzInput, FuzzTarget};

fn e<E: Display>(err: E) -> String {
    err.to_string()
}

/// The example profile cooked, or nothing (which fails the fixture test) if the cook broke.
fn cooked_fixture() -> Vec<u8> {
    IesFile::parse(&example_text())
        .and_then(|f| CookedIesProfile::from_file(&f))
        .map(|c| c.to_bytes())
        .unwrap_or_default()
}

/// Parse, resample (when the file has light) and re-encode as LM-63-2002 text.
fn ies_load(s: &str) -> Result<String, String> {
    let file = IesFile::parse(s).map_err(e)?;
    // The resample runs on every accepted file: a dark profile is a rejection, not a crash.
    let _ = CookedIesProfile::from_file(&file);
    Ok(file.to_text())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![
        text(
            "ies",
            Some("ies-profile"),
            IES_PROFILE_FORMAT_VERSION,
            true,
            example_text,
            ies_load,
        ),
        FuzzTarget {
            name: "ies-profile",
            format: Some("ies-profile"),
            format_version: IES_PROFILE_FORMAT_VERSION,
            round_trip: true,
            input: FuzzInput::Bytes {
                fixture: cooked_fixture,
                load: |b| {
                    let profile = CookedIesProfile::from_bytes(b).map_err(e)?;
                    let _ = profile.relative([0.3, -0.2, -0.9]);
                    Ok(profile.to_bytes())
                },
            },
        },
    ]
}
