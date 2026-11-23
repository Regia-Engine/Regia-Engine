// SPDX-License-Identifier: MIT-0
//! The digest trace (`run_traced`, `run_traffic_traced`, `nav_world_lab --trace`) reports the same
//! run as `run`: the report is unchanged, every section reports once per step in run order, and the
//! last traffic line is the report's digest. Cross-host comparisons diff these traces for the first
//! diverging step, so a trace that skipped or reordered a step would point at the wrong one.

use nav_world_lab::traffic::{run_traffic, run_traffic_traced};
use nav_world_lab::{LabConfig, run, run_traced};

fn short() -> LabConfig {
    LabConfig {
        crowd_agents: 4,
        load_at: 40,
        crowd_steps: 80,
        lane_agents: 6,
        lane_steps: 60,
        ragdoll_steps: 120,
    }
}

#[test]
fn the_trace_reports_every_step_of_the_same_run() {
    let config = short();
    let plain = run(&config).expect("run");
    let mut lines: Vec<(String, u32, u64)> = Vec::new();
    let traced = run_traced(&config, &mut |s, step, d| lines.push((s.to_owned(), step, d))).expect("traced");
    assert_eq!(plain, traced, "tracing changed the run");

    let sections: Vec<&str> = {
        let mut v: Vec<&str> = lines.iter().map(|(s, _, _)| s.as_str()).collect();
        v.dedup();
        v
    };
    assert_eq!(
        sections,
        ["cook", "crowd", "lanes", "ragdoll"],
        "sections out of run order"
    );
    for (section, count) in [
        ("crowd", config.crowd_steps),
        ("lanes", config.lane_steps),
        ("ragdoll", 1),
    ] {
        let steps: Vec<u32> = lines
            .iter()
            .filter(|(s, _, _)| s == section)
            .map(|l| l.1)
            .collect();
        assert_eq!(
            steps,
            (0..count).collect::<Vec<_>>(),
            "{section}: a step was skipped or repeated"
        );
    }
    assert!(
        lines.iter().any(|(s, _, _)| s == "cook"),
        "no cooked column reported"
    );
}

#[test]
fn the_traffic_trace_ends_at_the_report_digest() {
    let plain = run_traffic(90).expect("traffic");
    let mut lines: Vec<(String, u32, u64)> = Vec::new();
    let traced = run_traffic_traced(90, &mut |s, step, d| lines.push((s.to_owned(), step, d)))
        .expect("traced traffic");
    assert_eq!(plain, traced, "tracing changed the traffic run");
    let traffic: Vec<&(String, u32, u64)> = lines.iter().filter(|l| l.0 == "traffic").collect();
    let physics = lines.iter().filter(|l| l.0 == "traffic_physics").count();
    assert_eq!(traffic.len(), 90);
    assert_eq!(physics, 90);
    assert_eq!(traffic.last().map(|l| l.2), Some(plain.digest));
}
