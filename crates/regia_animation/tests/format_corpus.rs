// SPDX-License-Identifier: MPL-2.0
//! Cooked clips against the old-bytes corpus (ADR-2743): `crates/regia_agent/tests/fixtures/format-corpus/cooked-clip/`.
//! The clip lives in `regia_animation`, above the registry's crate (`COVERED_ELSEWHERE`), so the four checks run
//! here. The version is the payload's, behind the cooked header: it is read and patched through the clip codec.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_animation::clip::CLIP_FORMAT_VERSION;
use regia_animation::cooked::{cooked_clip_file_version, migrate_cooked_clip_file};
use regia_asset::decode_cooked_file;

/// Decode and re-encode at the current payload version (what `upgrade-project` does, header kept).
fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    cooked_clip_file_version(bytes)?.ok_or_else(|| "not a cooked clip".to_string())?;
    migrate_cooked_clip_file(bytes)
}

fn read(bytes: &[u8]) -> Option<u32> {
    cooked_clip_file_version(bytes).ok().flatten()
}

/// A payload from v2 on starts with the `FCLP` magic and the version word.
fn patch(bytes: &[u8], version: u32) -> Option<Vec<u8>> {
    let (_, payload) = decode_cooked_file(bytes).ok()?;
    if !payload.starts_with(b"FCLP") {
        return None;
    }
    let at = bytes.len() - payload.len() + 4;
    let mut out = bytes.to_vec();
    out.get_mut(at..at + 4)?.copy_from_slice(&version.to_le_bytes());
    Some(out)
}

#[test]
fn bless_missing_current_fixtures() {
    if format_corpus_support::blessing() {
        format_corpus_support::bless_migrated("cooked-clip", CLIP_FORMAT_VERSION, "fckd", &load);
    }
}

#[test]
fn every_cooked_clip_fixture_loads_round_trips_and_refuses_the_future() {
    let problems =
        format_corpus_support::check_format_with("cooked-clip", CLIP_FORMAT_VERSION, &load, &read, &patch);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The production runtime loader, not only the `upgrade-project` pair: every fixture loads through
/// `load_cooked_clip`, and a copy patched to the next version is refused with an error naming the version
/// (a payload written by a newer engine must not fail with postcard's opaque "bad encoding" text).
#[test]
fn the_runtime_clip_loader_refuses_the_future_naming_the_version() {
    let mut problems = Vec::new();
    for entry in format_corpus_support::load_all_indexes()
        .remove("cooked-clip")
        .expect("the cooked-clip corpus folder")
    {
        let tag = format!("cooked-clip v{}", entry.version);
        let path = format_corpus_support::corpus_dir()
            .join("cooked-clip")
            .join(&entry.file);
        let bytes = std::fs::read(&path).expect("read a fixture");
        if let Err(e) = regia_animation::cooked::load_cooked_clip(&bytes) {
            problems.push(format!("{tag}: the runtime loader rejects the fixture: {e}"));
            continue;
        }
        let future = match patch(&bytes, CLIP_FORMAT_VERSION + 1)
            .or_else(|| load(&bytes).ok().and_then(|m| patch(&m, CLIP_FORMAT_VERSION + 1)))
        {
            Some(f) => f,
            None => {
                problems.push(format!("{tag}: no way to patch the version field"));
                continue;
            }
        };
        match regia_animation::cooked::load_cooked_clip(&future) {
            Ok(_) => problems.push(format!("{tag}: a copy at the next version loaded silently")),
            Err(e) if !format_corpus_support::names_the_version(&e.to_string()) => problems.push(format!(
                "{tag}: the runtime loader refused the future without naming the version: {e}"
            )),
            Err(_) => {}
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
