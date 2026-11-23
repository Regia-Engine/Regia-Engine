// SPDX-License-Identifier: MPL-2.0
//! Spec 96 (ADR-1704): every method the editor registers on `AgentExtension` has a row in the
//! extension table of `book/COVERAGE.md` naming `regia_editor`, and a page that names it.
//!
//! This is the runtime side of `cargo xtask book-coverage`, which finds the same methods by
//! scanning the source: a method the scan missed would still fail here.

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;

#[test]
fn every_editor_extension_method_has_a_book_page() {
    let mut world = World::new();
    regia_editor::agent_ext::install(&mut world);
    // `editor.headless_status`, `editor.open_level`, `agent.shutdown`, `gen.*` (ADR-2412).
    regia_editor::headless::install(&mut world);
    let names = world.resource::<AgentExtension>().method_names();
    assert!(
        names.len() > 50,
        "the editor registers its methods: {}",
        names.len()
    );
    let gaps = regia_test_utils::book::extension_gaps(
        &regia_test_utils::book::workspace_root(),
        &names,
        "regia_editor",
        &regia_agent::mentions_token,
    );
    assert!(
        gaps.is_empty(),
        "undocumented editor methods:\n{}",
        gaps.join("\n")
    );
}
