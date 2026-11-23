// SPDX-License-Identifier: MIT-0
//! The hitch-soak bundle loader (`lookdev::soak_bundle`): layout check, read-only guard,
//! deterministic camera paths and the per-frame hitch CSV. CPU only; the fixture is a tiny
//! synthetic bundle written into a unique temp dir.

use lookdev::chase::ChaseSample;
use lookdev::soak::SoakCut;
use lookdev::soak_bundle::{Bundle, CameraPath, hitch_csv, write_hitch_csv};

fn fixture() -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir("soak_bundle");
    for map in ["bore_twelve", "switchbacks"] {
        std::fs::create_dir_all(root.join(map)).expect("fixture dir");
        std::fs::write(root.join(map).join("level.ron"), "()").expect("fixture level");
    }
    // A folder without a level is not a map.
    std::fs::create_dir_all(root.join("notes")).expect("fixture dir");
    root
}

#[test]
fn open_lists_the_maps_sorted() {
    let root = fixture();
    let bundle = Bundle::open(&root).expect("bundle");
    assert_eq!(bundle.maps(), ["bore_twelve", "switchbacks"]);
    assert!(bundle.has_map("switchbacks"));
    assert!(!bundle.has_map("notes"));
    assert_eq!(bundle.root(), root.as_path());
}

/// A DO27 content root (`REGIA_DO27_CONTENT`): the maps sit under `maps/`, which is where the
/// chase loads them from. `do27_chase --content <root>` and the nightly sweep pass this form.
#[test]
fn open_accepts_a_do27_content_root_with_maps_under_maps() {
    let root = regia_test_utils::unique_temp_dir("soak_bundle_content_root");
    for map in ["switchbacks", "bore_twelve"] {
        let dir = root.join("maps").join(map);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("level.ron"), "()").expect("fixture level");
    }
    std::fs::create_dir_all(root.join("props")).expect("fixture dir");
    let bundle = Bundle::open(&root).expect("content root opens");
    assert_eq!(bundle.maps(), ["bore_twelve", "switchbacks"]);
    assert_eq!(bundle.root(), root.as_path());
    // The whole content root stays read-only, not only `maps/`.
    let inside = root.join("props").join("hitch.csv");
    assert!(write_hitch_csv(&bundle, &inside, &[], 1.0).is_err());
    assert!(!inside.exists());
}

#[test]
fn open_rejects_a_maps_folder_without_levels() {
    let root = regia_test_utils::unique_temp_dir("soak_bundle_empty_maps");
    std::fs::create_dir_all(root.join("maps").join("switchbacks")).expect("fixture dir");
    assert!(Bundle::open(&root).is_err());
}

#[test]
fn open_rejects_a_missing_or_empty_bundle() {
    let root = regia_test_utils::unique_temp_dir("soak_bundle_empty");
    assert!(Bundle::open(&root).is_err());
    assert!(Bundle::open(&root.join("absent")).is_err());
}

#[test]
fn the_bundle_is_never_written() {
    let root = fixture();
    let bundle = Bundle::open(&root).expect("bundle");
    let inside = root.join("bore_twelve").join("hitch.csv");
    assert!(write_hitch_csv(&bundle, &inside, &[], 1.0).is_err());
    assert!(!inside.exists());
    let outside = regia_test_utils::unique_temp_dir("soak_bundle_out").join("hitch.csv");
    write_hitch_csv(&bundle, &outside, &[ChaseSample::default()], 1.0).expect("write");
    assert!(outside.is_file());
}

#[test]
fn camera_paths_are_deterministic_and_close_their_lap() {
    for name in ["orbit", "line", "figure8"] {
        let path = CameraPath::parse(name).expect("known path");
        assert_eq!(path.pose(17, 500.0), path.pose(17, 500.0), "{name}");
        let a = path.pose(0, 500.0);
        let b = path.pose(CameraPath::LAP_FRAMES, 500.0);
        for k in 0..3 {
            assert!((a.0[k] - b.0[k]).abs() < 1e-3, "{name} eye {k}");
        }
        assert_ne!(path.pose(0, 500.0), path.pose(300, 500.0), "{name} moves");
    }
    assert!(CameraPath::parse("spiral").is_err());
}

#[test]
fn waypoints_are_fixed_cameras() {
    let cuts = CameraPath::Orbit.waypoints(6, 400.0);
    assert_eq!(cuts.len(), 6);
    assert!(cuts.iter().all(|c| matches!(c, SoakCut::Fixed(..))));
}

#[test]
fn hitch_csv_flags_frames_over_twice_the_median() {
    let sample = |wall_ms, events| ChaseSample {
        wall_ms,
        events,
        ..ChaseSample::default()
    };
    let csv = hitch_csv(&[sample(4.0, 0), sample(4.0, 0), sample(9.0, 3)], 4.0);
    let rows: Vec<Vec<&str>> = csv.lines().map(|l| l.split(',').collect()).collect();
    assert_eq!(rows[0], ["frame", "t_s", "wall_ms", "ratio", "hitch", "events"]);
    assert!(
        rows.iter().all(|r| r.len() == 6),
        "events must not contain commas"
    );
    assert_eq!(rows[1][4], "0");
    assert_eq!(rows[3][4], "1");
    assert_eq!(rows[3][2], "9.000");
}
