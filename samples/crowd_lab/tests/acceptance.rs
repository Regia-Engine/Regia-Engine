// SPDX-License-Identifier: MIT-0
//! Spec 85 acceptance: the crowd lab is deterministic (two runs, and a serial-chunk run, give
//! the same digest), shares poses, retargets every rig without issues, and keeps planted feet
//! planted.

use crowd_lab::{ClipShape, LabConfig, run};

#[test]
fn the_crowd_lab_is_deterministic_and_clean() {
    let config = LabConfig::default();
    let first = run(&config).expect("first run");
    for line in first.lines() {
        println!("{line}");
    }
    let second = run(&config).expect("second run");
    assert_eq!(first, second, "two runs differ");
    let serial = run(&LabConfig {
        chunk: 1,
        ..config.clone()
    })
    .expect("chunk 1 run");
    assert_eq!(serial.digest, first.digest, "chunk size changed the output");
    assert_eq!(serial.nav_digest, first.nav_digest);

    assert_eq!(first.rigs.len(), 3);
    for rig in &first.rigs {
        assert!(rig.issues.is_empty(), "{}: {:?}", rig.name, rig.issues);
        assert!(
            rig.max_direction_error_deg < 0.1,
            "{}: {}",
            rig.name,
            rig.max_direction_error_deg
        );
    }
    assert!(
        (first.rigs[1].facing_yaw_deg.abs() - 180.0).abs() < 0.01,
        "the +Z rig is turned"
    );
    assert!(first.shared_hits > 0, "no pose was shared");
    assert!(
        first.evaluated_poses < first.updated,
        "sharing evaluated no fewer poses than updates"
    );
    assert!(first.skipped > 0, "no LOD interval skipped an update");
    assert!(
        first.skate_frames > 1000,
        "too few planted frames measured: {}",
        first.skate_frames
    );
    // Foot locking holds planted feet: below the spec's 0.1 m/s, and far below the ground speed.
    assert!(
        first.mean_skate < 0.1,
        "planted feet skate {} m/s",
        first.mean_skate
    );
    assert!(
        first.mean_skate < 0.15 * first.mean_speed,
        "planted feet skate {} m/s at {} m/s",
        first.mean_skate,
        first.mean_speed
    );
}

/// The same crowd on the clip shapes an importer hands over with its root-motion setting off:
/// the speeds are measured, the retargets stay clean and planted feet stay planted.
#[test]
fn clips_without_a_root_motion_track_walk_the_same_crowd() {
    for shape in [ClipShape::InPlace, ClipShape::TravellingHips] {
        let summary = run(&LabConfig {
            shape,
            ..LabConfig::default()
        })
        .expect("run");
        for line in summary.lines() {
            println!("{shape:?} {line}");
        }
        for rig in &summary.rigs {
            assert!(rig.issues.is_empty(), "{shape:?} {}: {:?}", rig.name, rig.issues);
        }
        assert!(
            summary.skate_frames > 1000,
            "{shape:?}: too few planted frames measured: {}",
            summary.skate_frames
        );
        assert!(
            summary.mean_skate < 0.1,
            "{shape:?}: planted feet skate {} m/s",
            summary.mean_skate
        );
    }
}

/// The digest trace (`run_traced`, `crowd_lab --trace`) reports the same run: one `nav`, one
/// `crowd` and one `crowd_motion` line per step, the last `nav` and `crowd` equal to the summary's
/// digests.
#[test]
fn the_trace_reports_every_step_of_the_same_run() {
    let config = LabConfig {
        agents: 24,
        steps: 40,
        ..LabConfig::default()
    };
    let plain = run(&config).expect("run");
    let mut lines: Vec<(String, u32, u64)> = Vec::new();
    let traced = crowd_lab::run_traced(&config, &mut |s, step, d| lines.push((s.to_owned(), step, d)))
        .expect("traced");
    assert_eq!(plain, traced, "tracing changed the run");
    for (section, last) in [("nav", plain.nav_digest), ("crowd", plain.digest)] {
        let rows: Vec<&(String, u32, u64)> = lines.iter().filter(|l| l.0 == section).collect();
        let steps: Vec<u32> = rows.iter().map(|l| l.1).collect();
        assert_eq!(
            steps,
            (0..40).collect::<Vec<u32>>(),
            "{section}: a step was skipped or repeated"
        );
        assert_eq!(
            rows.last().map(|l| l.2),
            Some(last),
            "{section}: last line is not the summary digest"
        );
    }
    let motion: Vec<u32> = lines
        .iter()
        .filter(|l| l.0 == "crowd_motion")
        .map(|l| l.1)
        .collect();
    assert_eq!(
        motion,
        (0..40).collect::<Vec<u32>>(),
        "crowd_motion: a step was skipped or repeated"
    );
}
