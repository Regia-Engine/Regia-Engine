// SPDX-License-Identifier: MPL-2.0
//! The player sweep's roster, launch plan and markdown table. No GPU and no ssh.

use std::path::PathBuf;

use xtask::player_sweep::{
    AdapterBrief, CacheMode, Intent, MachineSpec, Place, Reach, SweepRow, decide, gtx_1080_allowed, host_of,
    markdown, matching_index, parked_tail_seconds, plan_launches, replay_warmup, roster, with_size,
};

fn machine(id: &str) -> MachineSpec {
    roster().into_iter().find(|machine| machine.id == id).expect(id)
}

#[test]
fn gtx_1080_stays_skipped_until_marked_stable() {
    assert!(!gtx_1080_allowed(false));
    assert!(gtx_1080_allowed(true));
    let intent = decide(&machine("gtx-1080"), &[], Reach::NotStable, false, false);
    let Intent::Skip(note) = intent else {
        panic!("expected a skip");
    };
    assert!(note.contains("REGIA_156_STABLE"), "{note}");
}

#[test]
fn a_local_5080_runs_and_a_missing_one_skips() {
    let adapters = vec![AdapterBrief {
        index: 0,
        name: "NVIDIA GeForce RTX 5080".to_owned(),
    }];
    assert_eq!(matching_index(&adapters, "5080"), Some(0));
    let intent = decide(&machine("rtx-5080"), &adapters, Reach::RemoteOk, false, false);
    assert_eq!(intent, Intent::Run { gpu_index: Some(0) });
    let missing = decide(&machine("rtx-5080"), &[], Reach::RemoteOk, false, true);
    assert!(matches!(missing, Intent::Skip(_)));
}

#[test]
fn local_only_skips_remotes_without_calling_them_stable() {
    let arc = decide(&machine("arc"), &[], Reach::RemoteOk, false, true);
    assert_eq!(arc, Intent::Skip("local-only".to_owned()));
    assert_eq!(host_of(&machine("buildhost-5090"), None), "buildhost");
    assert_eq!(host_of(&machine("buildhost-5090"), Some("gpu-box")), "gpu-box");
    assert!(matches!(
        machine("buildhost-5090").place,
        Place::Remote { card: Some(5), .. }
    ));
}

#[test]
fn launches_cover_cold_replay_five_quits_and_a_crash() {
    let out = PathBuf::from("/tmp/regia-sweep");
    let launches = plan_launches(600.0, 5, false, &out);
    let cold = launches
        .iter()
        .find(|launch| launch.scenario == "cold")
        .expect("cold");
    assert_eq!(cold.cache, CacheMode::Empty);
    assert!(cold.args.windows(2).any(|pair| pair == ["--frames", "2"]));
    assert!(cold.needs_content);
    let replay = launches
        .iter()
        .find(|launch| launch.scenario == "replay")
        .expect("replay");
    assert_eq!(replay.cache, CacheMode::Warm);
    assert!(replay.args.windows(2).any(|pair| pair == ["--seconds", "600"]));
    assert_eq!(replay_warmup(600.0), 30);
    assert_eq!(replay_warmup(2.0), 2);
    assert!(replay.args.windows(2).any(|pair| pair == ["--warmup", "30"]));
    let quits = launches.iter().filter(|launch| launch.group == "quits").count();
    assert_eq!(quits, 5);
    let crash = launches
        .iter()
        .find(|launch| launch.scenario == "crash")
        .expect("crash");
    assert!(crash.args.iter().any(|arg| arg == "--crash-test"));
    assert!(!crash.needs_content);
    assert!(!crash.args.iter().any(|arg| arg == "--map"));
    let windowed = plan_launches(2.0, 1, true, &out);
    assert!(
        windowed
            .iter()
            .find(|launch| launch.scenario == "cold")
            .expect("cold")
            .args
            .iter()
            .any(|arg| arg == "--window")
    );
    assert!(
        !windowed
            .iter()
            .find(|launch| launch.scenario == "crash")
            .expect("crash")
            .args
            .iter()
            .any(|arg| arg == "--window")
    );
}

#[test]
fn the_table_names_every_machine_and_scenario() {
    let mut rows = Vec::new();
    for spec in roster() {
        for scenario in ["cold", "replay", "quits", "crash"] {
            rows.push(SweepRow {
                machine: spec.id.to_owned(),
                scenario: scenario.to_owned(),
                status: "skip".to_owned(),
                exit_code: String::new(),
                summary: String::new(),
                note: "test".to_owned(),
            });
        }
    }
    let text = markdown("2026-10-14", &rows);
    assert!(text.contains("# Player sweep 2026-10-14"));
    for id in ["rtx-5080", "buildhost-5090", "gtx-1080", "arc"] {
        assert!(text.contains(id), "{text}");
    }
    for scenario in ["cold", "replay", "quits", "crash"] {
        assert!(text.contains(scenario), "{text}");
    }
}

#[test]
fn size_goes_to_the_map_launches_only_and_a_bad_size_is_refused() {
    let mut launches = plan_launches(60.0, 2, false, &PathBuf::from("/out"));
    with_size(&mut launches, "1920x1080").expect("valid size");
    for launch in &launches {
        let has = launch.args.windows(2).any(|pair| pair == ["--size", "1920x1080"]);
        assert_eq!(
            has,
            launch.needs_content && !launch.args.is_empty(),
            "{}",
            launch.scenario
        );
    }
    let mut again = plan_launches(60.0, 0, false, &PathBuf::from("/out"));
    assert!(with_size(&mut again, "1080p").is_err());
    assert!(with_size(&mut again, "0x1080").is_err());
    assert!(with_size(&mut again, "1920x").is_err());
}

#[test]
fn a_replay_past_the_route_end_reports_its_parked_tail() {
    assert_eq!(parked_tail_seconds(100.0), 0.0);
    assert_eq!(parked_tail_seconds(115.2), 0.0);
    assert!((parked_tail_seconds(120.0) - 4.8).abs() < 1e-9);
    assert!((parked_tail_seconds(600.0) - 484.8).abs() < 1e-9);
}
