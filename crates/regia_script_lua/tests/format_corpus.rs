// SPDX-License-Identifier: MPL-2.0
//! `luamod` and `luastate` against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/`.
//! The Lua host depends on the registry's crate (`COVERED_ELSEWHERE`), so the four checks run here, through the two
//! fuzz targets this crate adds to the registry.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_agent::fuzz_targets::{FuzzInput, FuzzTarget};
use regia_script_lua::fuzz::fuzz_targets;

fn target(format: &str) -> FuzzTarget {
    fuzz_targets()
        .into_iter()
        .find(|t| t.format == Some(format))
        .unwrap_or_else(|| panic!("no {format} target"))
}

fn loader(t: &FuzzTarget) -> impl Fn(&[u8]) -> Result<Vec<u8>, String> {
    let input = t.input;
    move |bytes: &[u8]| match input {
        FuzzInput::Text { load, .. } => {
            let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            load(text).map(String::into_bytes)
        }
        FuzzInput::Bytes { load, .. } => load(bytes),
    }
}

fn fixture_of(t: &FuzzTarget) -> Vec<u8> {
    match t.input {
        FuzzInput::Text { fixture, .. } => fixture().into_bytes(),
        FuzzInput::Bytes { fixture, .. } => fixture(),
    }
}

#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    for (format, ext) in [("luamod", "ron"), ("luastate", "bin")] {
        let t = target(format);
        format_corpus_support::bless_current(
            format,
            t.format_version,
            ext,
            &fixture_of(&t),
            "frozen copy of the fuzz-target fixture of regia_script_lua::fuzz",
        );
    }
}

#[test]
fn every_lua_fixture_loads_round_trips_and_refuses_the_future() {
    let mut problems = Vec::new();
    for format in ["luamod", "luastate"] {
        let t = target(format);
        problems.extend(format_corpus_support::check_format(
            format,
            t.format_version,
            &loader(&t),
        ));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
