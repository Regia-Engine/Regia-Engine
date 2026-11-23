// SPDX-License-Identifier: MPL-2.0
//! `.bake.ron` under seeded mutation (ADR-1917): `GiBakeDescription::parse_versioned` and `validate` return a
//! value or an error, never panic or hang. The fixture is the example from `docs/formats/bake-ron.md`. This is
//! the registry's `bake` target: `regia_pt` is an optional dependency of `regia_agent`, so the registry test
//! (`regia_agent/tests/fuzz_targets.rs`) names this test for it.

use regia_pt::bake_desc::{GiBakeDescription, migrate_bake_ron};
use regia_test_utils::fuzz::fuzz_text_loader;

fn parse_and_validate(text: &str) -> Result<(), String> {
    let (desc, _version) = GiBakeDescription::parse_versioned(text).map_err(|e| e.to_string())?;
    desc.validate().map_err(|e| e.to_string())
}

#[test]
fn bake_desc_ron_never_panics() {
    let fixture = include_str!("fixtures/fuzz.bake.ron");
    parse_and_validate(fixture).expect("the documented example parses and validates");
    fuzz_text_loader(fixture, 0xBA4E, 2000, parse_and_validate);
}

/// The cell index of `regia bake-lightmaps --cells` (`cells.json`, ADR-3318) under seeded mutation:
/// `CellBakeIndex::parse` returns an index or an error, never panics. The registry's `lightmap-cells`
/// target (named in `regia_agent/tests/fuzz_targets.rs`).
#[test]
fn lightmap_cells_index_never_panics() {
    use regia_pt::lightmap::cells::{CellBakeIndex, cell_bake_index_file_version};
    let fixture = include_str!("fixtures/fuzz.cells.json");
    CellBakeIndex::parse(fixture).expect("the documented example parses");
    assert_eq!(cell_bake_index_file_version(fixture.as_bytes()), Ok(Some(1)));
    fuzz_text_loader(fixture, 0xCE11, 2000, |text| {
        let _ = cell_bake_index_file_version(text.as_bytes());
        CellBakeIndex::parse(text).map(|_| ()).map_err(|e| e.to_string())
    });
}

/// The on-disk migrator (`regia upgrade-project`, ADR-7818) scans user text to move the v2 fixture's `sun` into
/// `suns`: under the same mutations it returns text or an error, never panics or hangs, and whatever it writes
/// loads again at the current version.
#[test]
fn bake_desc_migration_never_panics() {
    let fixture = include_str!("fixtures/fuzz.bake.ron");
    migrate_bake_ron(fixture).expect("the documented v2 example migrates");
    fuzz_text_loader(fixture, 0x5045, 1000, |text: &str| {
        let migrated = migrate_bake_ron(text).map_err(|e| e.to_string())?;
        GiBakeDescription::parse_versioned(&migrated)
            .map(|_| ())
            .map_err(|e| format!("the migrated text does not load: {e}"))
    });
}
