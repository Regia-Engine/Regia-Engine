// SPDX-License-Identifier: MIT-0
//! ADR-2133 acceptance: the nav world lab is deterministic, its crowd waits at the streamed border
//! and arrives once the east loads, its paths go round the mud, its two-way lanes pass clear, its
//! lane preferences show, and its ragdoll settles.

use nav_world_lab::{LabConfig, run};

#[test]
fn the_nav_world_lab_is_deterministic_and_every_feature_shows() {
    let config = LabConfig::default();
    let first = run(&config).expect("first run");
    println!("{first}");
    let second = run(&config).expect("second run");
    assert_eq!(first, second, "two runs differ");

    assert!(
        first.waiting_before_load >= config.crowd_agents / 2,
        "agents waited at the border: {}",
        first.waiting_before_load
    );
    assert_eq!(
        first.arrived, config.crowd_agents,
        "everyone arrived after the load"
    );
    assert!(first.tile_waits >= first.waiting_before_load as u64);
    assert!(
        first.mud_crossings < config.crowd_agents / 2,
        "most paths go round the mud: {}",
        first.mud_crossings
    );
    assert!(first.oncoming_yields > 0);
    assert!(
        first.closest_pass >= 0.6 - 1e-3,
        "walkers passed clear: {}",
        first.closest_pass
    );
    let [weighted, plain, flagged] = first.branch_counts;
    assert!(
        weighted > plain,
        "the weighted branch draws more: {:?}",
        first.branch_counts
    );
    assert!(
        flagged >= config.lane_agents / 5 / 2,
        "the preferring walkers took the flagged branch: {:?}",
        first.branch_counts
    );
    assert!(first.ragdoll_settled_at.is_some(), "the ragdoll came to rest");
}
