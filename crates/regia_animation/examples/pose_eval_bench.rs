// SPDX-License-Identifier: MPL-2.0
//! Cost of one side-effect-free pose evaluation of the demo net character (ADR-2551): what an
//! exact-tick lag-compensation rewind pays per broad-phase candidate and record.
//!
//! `cargo run --release -p regia_animation --example pose_eval_bench`

use std::time::Instant;

use regia_animation::components::Animator;
use regia_animation::pose::Pose;
use regia_animation::pose_eval::demo::{AIM, SPEED, demo_net_character};
use regia_animation::pose_eval::{
    AnimStateSnapshot, ParamLanes, PoseRequest, STATE_WORDS_VERSION, evaluate_pose,
};
use regia_ecs::prelude::Entity;

fn main() {
    let Some((skeleton, graph, clips)) = demo_net_character() else {
        eprintln!("the demo rig could not be built");
        return;
    };
    let mut animator = Animator::graph(graph, clips);
    animator.set(SPEED, 2.0);
    animator.set(AIM, 1.0);
    // A mid-walk state: [version, graph, machine current 0, flags (entered), blend space slots
    // (4 × [time, started]), jump clip [0, 0], layer clip [0.3, started]].
    let mut words = vec![STATE_WORDS_VERSION, 0, 0, 1, 4];
    for t in [0.4f32, 0.4, 0.4, 0.4] {
        words.extend([t.to_bits(), 2]);
    }
    words.extend([0, 0, 0.3f32.to_bits(), 2]);
    let state = AnimStateSnapshot::from_words(&words);
    let params = ParamLanes::capture(&animator.parameters, None).to_set();
    let request = PoseRequest {
        skeleton: &skeleton,
        source: &animator.source,
        state: &state,
        params: &params,
        delta: 1.0 / 60.0,
        rig: Entity::PLACEHOLDER,
        socket_targets: None,
    };
    let mut pose = Pose::default();
    if let Err(e) = evaluate_pose(&request, &mut pose) {
        eprintln!("evaluation failed: {e}");
        return;
    }
    let runs = 20_000u32;
    let start = Instant::now();
    for _ in 0..runs {
        let _ = evaluate_pose(&request, &mut pose);
    }
    let per = start.elapsed().as_secs_f64() * 1e6 / f64::from(runs);
    println!(
        "pose_eval: {} joints, {} words: {per:.2} us per evaluation ({runs} runs)",
        skeleton.len(),
        state.words().len()
    );
}
