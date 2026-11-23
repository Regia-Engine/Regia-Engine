// SPDX-License-Identifier: MPL-2.0
//! Targets for a format frozen by a showcase lane: `.ecology.ron` profiles beside biomes
//! (ADR-2456). The cooked voxel far proxy blob (`FVOX`, ADR-2445) has its own module
//! (`voxel_far`, v2 boxes, ADR-2655).

use regia_procgen::biome::def::BiomeDef;
use regia_procgen::biome::ecology::{ECOLOGY_FORMAT_VERSION, EcologyProfile, starter_text, validate_ecology};

use super::FuzzTarget;
use super::formats::text;

/// A shipped preset's profile, as `regia new` copies it into a project.
fn ecology_fixture() -> String {
    starter_text("starter/biomes/beech_forest.ecology.ron")
        .unwrap_or_default()
        .to_owned()
}

/// Parse (with migration), validate the profile against its shipped biome the way
/// `biome.validate_ecology` does (its species catalog, then the species-mix rules), then write the
/// canonical text.
fn ecology_load(text: &str) -> Result<String, String> {
    let profile = EcologyProfile::from_ron(text).map_err(|e| e.to_string())?;
    let biome = starter_text("starter/biomes/beech_forest.biome.ron")
        .ok_or_else(|| "bug: the beech_forest starter biome is missing".to_string())
        .and_then(|text| BiomeDef::from_ron(text).map_err(|e| format!("bug: starter biome: {e}")))?;
    let _ = validate_ecology(&biome, Some(&profile), &profile.catalog()).plausible();
    Ok(profile.to_ron())
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![text(
        "ecology",
        Some("ecology"),
        ECOLOGY_FORMAT_VERSION,
        true,
        ecology_fixture,
        ecology_load,
    )]
}
