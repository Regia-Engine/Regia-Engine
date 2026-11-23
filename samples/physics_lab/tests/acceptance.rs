// SPDX-License-Identifier: MIT-0
//! ADR-2424 acceptance: the physics lab is deterministic over N fixed steps and every feature shows.

use physics_lab::{LabConfig, run};

#[test]
fn the_physics_lab_is_deterministic_and_every_feature_shows() {
    let config = LabConfig::default();
    let first = run(&config).expect("first run");
    println!("{first}");
    let second = run(&config).expect("second run");
    assert_eq!(first, second, "two runs differ");

    assert!(first.stack_standing, "the tower fell");
    let broke = first.lever_broke_at.expect("the torque-limited lever broke");
    assert!(broke < config.steps, "it broke during the run: {broke}");
    assert!(first.control_lever_held, "the lever without a limit held");
    assert!(first.chain_drop > 1.0, "the chain hangs: {}", first.chain_drop);
    assert!(
        first.plain_slide < 0.2,
        "the default box holds on the ramp: {}",
        first.plain_slide
    );
    assert!(first.ice_slide > 1.0, "the ice box slides: {}", first.ice_slide);
    assert!(first.ragdoll_settled_at.is_some(), "the ragdoll came to rest");
    assert_eq!(
        (first.debug_lines, first.debug_collections),
        (0, 0),
        "the draw is off by default"
    );
}

#[test]
fn the_debug_draw_collects_lines_without_changing_the_simulation() {
    let off = run(&LabConfig::default()).expect("off");
    let on = run(&LabConfig {
        debug_draw: true,
        ..LabConfig::default()
    })
    .expect("on");
    assert!(on.debug_lines > 0 && on.debug_collections > 0, "{on}");
    assert_eq!(off.digest, on.digest, "drawing is read-only");
}

#[test]
fn a_higher_torque_limit_holds_the_lever() {
    let held = run(&LabConfig {
        break_torque: 5.0,
        ..LabConfig::default()
    })
    .expect("run");
    assert_eq!(held.lever_broke_at, None);
}

/// ADR-2971 acceptance: the course runs from one end to the other with every move, twice alike.
#[test]
fn the_traversal_course_runs_every_move_and_two_runs_are_equal() {
    use physics_lab::course::{BLOCK_TOP, LADDER_HEIGHT, LADDER_X, run_course};
    use regia_framework::traversal::TraversalKind;

    let first = run_course();
    println!("{first:?}");
    assert_eq!(first, run_course(), "two runs differ");
    assert!(first.finished_at.is_some(), "never reached the top: {first:?}");
    for kind in [
        TraversalKind::Swim,
        TraversalKind::Vault,
        TraversalKind::Mantle,
        TraversalKind::LadderEnter,
        TraversalKind::Ladder,
        TraversalKind::LadderExit,
    ] {
        assert!(first.kinds.contains(&kind), "{kind:?} is missing: {first:?}");
    }
    assert!(
        first.float_error <= 0.05,
        "floats at the line: {}",
        first.float_error
    );
    assert!(first.position.x > LADDER_X, "{}", first.position);
    assert!(
        first.position.y > BLOCK_TOP + LADDER_HEIGHT,
        "on the upper floor: {}",
        first.position
    );
}
