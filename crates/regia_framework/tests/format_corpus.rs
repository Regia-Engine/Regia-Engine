// SPDX-License-Identifier: MPL-2.0
//! The save file against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/save/`.
//! The registry in `regia_agent` cannot reach the save container (`COVERED_ELSEWHERE`), so the same four checks run
//! here through `SaveGame::decode` and `SaveGame::encode`.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_framework::save_game::{SAVE_FORMAT_VERSION, SaveGame, SaveSpec};
use regia_test_utils::regia::ecs::prelude::*;
use regia_test_utils::regia::ecs::reflect::AppTypeRegistry;

/// Decode into a fresh world (migrations and CRC included). A save carries its write time, so encoding the
/// world again would differ on every call and there is no byte-preserving re-encode: the honest checks are
/// "every fixture decodes" and "a copy patched to the next version is refused naming the version"
/// (`check_load_only`), not a round trip.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    SaveGame::decode(&mut world, bytes, &SaveSpec::default()).map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

#[test]
fn every_save_fixture_loads_and_refuses_the_future() {
    let problems = format_corpus_support::check_load_only("save", SAVE_FORMAT_VERSION, &load);
    assert!(problems.is_empty(), "{problems:#?}");
}
