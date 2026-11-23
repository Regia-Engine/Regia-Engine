// SPDX-License-Identifier: MPL-2.0
//! Spec 96 (ADR-1704): the methods `regia_agent` itself registers on `AgentExtension` (outside the
//! runtime schema) have a row in the extension table of `book/COVERAGE.md` and a page.

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;

#[test]
fn every_agent_crate_extension_method_has_a_book_page() {
    let mut world = World::new();
    regia_agent::cloth::install_cloth_methods(&mut world);
    regia_agent::abilities::install_ability_methods(&mut world);
    regia_agent::choosers::install_chooser_methods(&mut world);
    regia_agent::curve_tables::install_curve_table_methods(&mut world);
    regia_agent::breadcrumbs::install_breadcrumb_methods(&mut world);
    regia_agent::trace::install_trace_methods(&mut world);
    let names = world.resource::<AgentExtension>().method_names();
    let cloth: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| n.starts_with("physics.cloth_"))
        .collect();
    assert_eq!(
        cloth,
        [
            "physics.cloth_stats",
            "physics.cloth_inspect",
            "physics.cloth_set_params",
            "physics.cloth_pin",
            "physics.cloth_unpin",
            "physics.cloth_wind",
            "physics.cloth_step",
            "physics.cloth_reset",
            "physics.cloth_edit_map",
            "physics.cloth_presets",
            "physics.cloth_set_layers",
            "physics.cloth_health",
        ]
    );
    for prefix in [
        "abilities.",
        "chooser.",
        "curves.",
        "render.breadcrumbs",
        "telemetry.",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(prefix)),
            "no `{prefix}` method installed: {names:?}"
        );
    }
    let gaps = regia_test_utils::book::extension_gaps(
        &regia_test_utils::book::workspace_root(),
        &names,
        "regia_agent",
        &regia_agent::mentions_token,
    );
    assert!(gaps.is_empty(), "undocumented methods:\n{}", gaps.join("\n"));
}
