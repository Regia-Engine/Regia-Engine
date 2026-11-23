// SPDX-License-Identifier: MPL-2.0
//! `release-signature` (the detached release-manifest signature sidecar) against the old-bytes corpus
//! (ADR-2743): the folder `crates/regia_agent/tests/fixtures/format-corpus/release-signature/`.
//! `regia_release_chunks` is a pipeline crate the runtime's fuzz-target registry must not link
//! (`COVERED_ELSEWHERE`, ADR-10356), so the four checks run here, on the reader that `regia release
//! verify` and the editor's `release.*` agent methods use: the fixture loads, the re-encoding reads back
//! equal, migrating is idempotent, and a copy at `format` 2 is refused with an error that names the
//! version.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_release_chunks::{RELEASE_SIGNATURE_FORMAT, canonical_signature_json};

fn signature(bytes: &[u8]) -> Result<Vec<u8>, String> {
    canonical_signature_json(bytes).map_err(|err| format!("{} ({})", err.message, err.code))
}

#[test]
fn every_release_signature_fixture_loads_round_trips_and_refuses_the_future() {
    let problems =
        format_corpus_support::check_format("release-signature", RELEASE_SIGNATURE_FORMAT, &signature);
    assert!(problems.is_empty(), "{problems:#?}");
}
