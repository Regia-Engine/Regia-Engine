// SPDX-License-Identifier: MIT-0
//! Pause-pattern independence of an active jump blend's decay (engine behavior marker).
//!
//! The motion-match player's inertializer seeds a jump blend's decay curve from the previous
//! step's output (`Inertializer::start`, spec 52 § 2.9). Before ADR-4411 the player ran in
//! `Update`, so a zero-delta host update (an agent-paused frame) produced an output at the blend's
//! un-advanced time, and that output seeded the next jump's decay: the pose one stepped frame after
//! a jump differed by about a centimeter between pause patterns, and `tests/pause_step.rs` exempted
//! poses from its bit-for-bit check while a blend ran.
//!
//! Since ADR-4411 (53f82c3c2, "step motion matching on simulation ticks") the predictor, matcher,
//! player and inertialization run in `FixedUpdate`. A paused update releases no fixed step, so the
//! player produces no output there and the seed is the same whatever the pause pattern. Measured on
//! x86_64 (the build server, 2026-11-11): every jump a running blend overlaps in the first 1500 steps
//! (steps 38, 698, 1358) gives the same pose with and without three paused updates before it, and
//! the 212-step scenario this file used to pin has no blend running at the pause at all (also at
//! the commit that restored it, 1837371c2). `tests/pause_step.rs` therefore compares poses bit for
//! bit in every state again.
//!
//! This test pins that behavior: it finds the jumps a running blend overlaps (so the scenario
//! follows the matcher on every instruction set instead of a hard-coded step), requires at least
//! one, and asserts the pose one stepped frame after each is identical across pause patterns. It
//! also guards the frozen-while-paused guarantee the sample relies on. If an engine change makes
//! the decay pause-pattern dependent again, this fails, and `tests/pause_step.rs` must get its
//! blending exemption back (spec 52 § 7 classifies inertialization as tolerance-based).

use mm_locomotion::{headless_app, hero};
use regia_animation::motion_matching::MotionMatchPlayer;

/// Fixed steps searched for jumps that a running blend overlaps.
const SEARCH_STEPS: usize = 800;

fn player_pose_bits(app: &mut regia_app::App) -> Vec<u32> {
    let world = app.world_mut();
    let entity = hero(world).expect("hero");
    world
        .get::<MotionMatchPlayer>(entity)
        .expect("player")
        .pose()
        .translations
        .iter()
        .flat_map(|v| v.to_array())
        .map(f32::to_bits)
        .collect()
}

/// Run `before` fixed steps, then `n` zero-delta frames while paused, then one stepped frame;
/// return the player pose bits after the next stepped frame.
fn pose_after_pause_pattern(before: usize, zero_delta_frames: usize) -> Vec<u32> {
    let mut app = headless_app();
    for _ in 0..before {
        app.update();
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = true;
    let frozen = player_pose_bits(&mut app);
    for _ in 0..zero_delta_frames {
        app.update();
        // The output pose stays bit for bit frozen while paused.
        assert_eq!(
            player_pose_bits(&mut app),
            frozen,
            "a paused host update moved the player pose"
        );
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = false;
    // The first stepped frame is the jump itself; a pause-dependent decay seed would show on the
    // next stepped frame, while the blend still runs.
    app.update();
    app.update();
    player_pose_bits(&mut app)
}

/// The steps after which a jump follows while a blend is still running.
fn blend_overlapped_jump_frames() -> Vec<usize> {
    let mut app = headless_app();
    let mut last = (0u64, false);
    let mut frames = Vec::new();
    for step in 1..=SEARCH_STEPS {
        app.update();
        let world = app.world_mut();
        let entity = hero(world).expect("hero");
        let player = world.get::<MotionMatchPlayer>(entity).expect("player");
        let now = (player.jumps_seen(), player.is_blending());
        if now.0 > last.0 && last.1 {
            frames.push(step - 1);
        }
        last = now;
    }
    frames
}

#[test]
fn an_active_blend_decay_is_the_same_whatever_the_pause_pattern_before_the_jump() {
    let frames = blend_overlapped_jump_frames();
    assert!(
        !frames.is_empty(),
        "no jump overlaps a running blend in {SEARCH_STEPS} steps: the scenario no longer exercises the decay seed"
    );
    for before in frames {
        assert_eq!(
            pose_after_pause_pattern(before, 0),
            pose_after_pause_pattern(before, 3),
            "after {before} steps the blend decay depends on the pause pattern again; restore the \
             blending exemption in pause_step.rs"
        );
    }
}

/// Diagnostic (macOS pass, ADR-5550; run with `--ignored --nocapture`): for every fixed step from 150 to 400 that a
/// jump follows, this prints whether a blend ran at the pause and whether the two pause patterns differ, so the
/// behavior pinned above can be checked per instruction set from evidence.
#[test]
#[ignore = "diagnostic: prints the jump frames and the pause-pattern sensitivity on this host"]
fn print_the_pause_sensitive_jump_frames() {
    let player = |app: &mut regia_app::App| {
        let world = app.world_mut();
        let entity = hero(world).expect("hero");
        let p = world.get::<MotionMatchPlayer>(entity).expect("player");
        (p.jumps_seen(), p.is_blending())
    };
    let mut probe = headless_app();
    let mut last = (0, false);
    for step in 1..=400usize {
        probe.update();
        let now = player(&mut probe);
        if now.0 > last.0 && step > 150 {
            let before = step - 1;
            let differ = pose_after_steps(before, 0) != pose_after_steps(before, 3);
            eprintln!(
                "jump at step {step}: blending at {before} {}, pause patterns differ {differ}",
                last.1
            );
        }
        last = now;
    }
}

/// [`pose_after_pause_pattern`] after `before` fixed steps instead of 212.
fn pose_after_steps(before: usize, zero_delta_frames: usize) -> Vec<u32> {
    let mut app = headless_app();
    for _ in 0..before {
        app.update();
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = true;
    for _ in 0..zero_delta_frames {
        app.update();
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = false;
    app.update();
    app.update();
    let world = app.world_mut();
    let entity = hero(world).expect("hero");
    world
        .get::<regia_animation::motion_matching::MotionMatchPlayer>(entity)
        .expect("player")
        .pose()
        .translations
        .iter()
        .flat_map(|v| v.to_array())
        .map(f32::to_bits)
        .collect()
}
