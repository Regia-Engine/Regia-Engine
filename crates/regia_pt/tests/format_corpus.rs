// SPDX-License-Identifier: MPL-2.0
//! `.bake.ron` against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/bake/`.
//! `regia_pt` is an optional dependency of the registry's crate (`COVERED_ELSEWHERE`), so the four checks run here.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_pt::bake_desc::{BAKE_DESC_FORMAT_VERSION, GiBakeDescription};

/// Parse (running the migration chain) and validate. The description only derives `Deserialize`, so there
/// is no re-encode: the honest checks are "every fixture parses and validates" and "a copy patched to the
/// next version is refused naming the version" (`check_load_only`), not a round trip.
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let (desc, _found) = GiBakeDescription::parse_versioned(text).map_err(|e| e.to_string())?;
    desc.validate().map_err(|e| e.to_string())?;
    Ok(bytes.to_vec())
}

#[test]
fn every_bake_fixture_loads_and_refuses_the_future() {
    let problems = format_corpus_support::check_load_only("bake", BAKE_DESC_FORMAT_VERSION, &load);
    assert!(problems.is_empty(), "{problems:#?}");
}
