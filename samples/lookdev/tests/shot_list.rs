// SPDX-License-Identifier: MIT-0
//! Lavapipe shot list at [`lookdev::LookdevConfig::tiny`]: every camera, sane measurements.

use lookdev::measure::PassMillis;
use lookdev::report::{self, ShotRecord};
use lookdev::{LookdevConfig, Planet};

#[test]
fn tiny_shot_list_measures_both_planets() {
    let device = regia_render::RenderDevice::headless().expect("lavapipe headless device");
    let dir = std::env::temp_dir().join(format!("regia-lookdev-shots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = LookdevConfig::tiny();
    let report = report::render_shot_list(&device, &config, &dir, |_| {}).expect("render shot list");

    assert_eq!(report.version, report::REPORT_VERSION);
    assert_eq!(report.shots.len(), 48, "8 cameras × 3 profiles × 2 planets");
    assert!(report.features.authored_layer_height_maps);
    assert!(report.features.authored_macro_variation);
    assert!(report.features.cdlod);
    // M15b is linked. Presence is not the terrain draw: every profile still applies one cascade.
    assert!(report.features.ssr_in_linked_crates);
    assert!(report.features.gi_probes_in_linked_crates);
    assert!(report.features.uploaded_layer_height_maps);
    assert!(report.features.uploaded_macro_variation);
    let macro_edge = u64::from(config.macro_resolution);
    assert_eq!(report.features.uploaded_macro_bytes, macro_edge * macro_edge * 4);
    let layer_edge = u64::from(config.layer_height_resolution);
    assert_eq!(
        report.features.uploaded_layer_height_bytes,
        6 * layer_edge * layer_edge * 4
    );
    assert!(
        report
            .features
            .profiles
            .iter()
            .all(|profile| profile.applied_terrain_cascades == 1)
    );
    let ultra = report
        .features
        .profiles
        .iter()
        .find(|profile| profile.profile == "ultra")
        .expect("ultra profile");
    assert!(ultra.height_fog && ultra.sun_shadow);

    for shot in &report.shots {
        assert_sane(shot, report.gpu_timing);
        assert!(
            shot.blown_pct < 40.0,
            "{} is blown ({:.2}%)",
            shot.id,
            shot.blown_pct
        );
        if GROUND_CAMERAS.contains(&shot.camera.as_str()) {
            assert!(
                shot.ground_luminance > 36.0,
                "{} ground is black ({:.2})",
                shot.id,
                shot.ground_luminance
            );
        }
    }

    let close = find(&report.shots, "europa/close_1m/ultra");
    let deck = find(&report.shots, "europa/close_1m/deck-medium");
    let sky = find(&report.shots, "europa/sky_heavy/ultra");
    assert!(
        deck.width < close.width,
        "deck {} vs ultra {}",
        deck.width,
        close.width
    );
    assert!(
        close.sharpness > 0.002,
        "close shot should show ground detail, sharpness {}",
        close.sharpness
    );
    assert!(
        close.contrast > 0.5,
        "close shot contrast {} is a flat frame",
        close.contrast
    );
    assert!(
        (sky.mean_luminance - close.mean_luminance).abs() > 1.0
            || report
                .shots
                .iter()
                .any(|shot| (shot.mean_luminance - close.mean_luminance).abs() > 1.0),
        "shot luminances are identical; close {} sky {}",
        close.mean_luminance,
        sky.mean_luminance
    );

    let mars = report.shots.iter().any(|shot| shot.planet == Planet::Mars.name());
    assert!(mars);

    let reread = report::read_report(&dir).expect("shots.json");
    assert_eq!(reread.shots.len(), report.shots.len());
    let same = report::compare_reports(&report, &reread);
    assert!(same.contains("missing_in_new: 0"), "{same}");
    assert!(same.contains("features: unchanged"), "{same}");

    let _ = std::fs::remove_dir_all(&dir);
}

const GROUND_CAMERAS: &[&str] = &["close_1m", "mid_50m", "low_sun", "noon", "road_bend"];

fn find<'a>(shots: &'a [ShotRecord], id: &str) -> &'a ShotRecord {
    shots.iter().find(|shot| shot.id == id).expect(id)
}

fn assert_sane(shot: &ShotRecord, gpu_timing: bool) {
    assert!(
        shot.mean_luminance.is_finite() && (0.0..=255.0).contains(&shot.mean_luminance),
        "{} luminance {}",
        shot.id,
        shot.mean_luminance
    );
    assert!(
        shot.contrast.is_finite() && (0.0..=255.0).contains(&shot.contrast),
        "{} contrast {}",
        shot.id,
        shot.contrast
    );
    assert!(
        (0.0..=100.0).contains(&shot.near_black_pct),
        "{} near-black {}",
        shot.id,
        shot.near_black_pct
    );
    assert!(
        (0.0..=100.0).contains(&shot.blown_pct),
        "{} blown {}",
        shot.id,
        shot.blown_pct
    );
    assert!(
        shot.saturation.is_finite() && (0.0..=1.0).contains(&shot.saturation),
        "{} saturation {}",
        shot.id,
        shot.saturation
    );
    assert!(
        shot.tiling_peak.is_finite() && (0.0..=1.0).contains(&shot.tiling_peak),
        "{} tiling {}",
        shot.id,
        shot.tiling_peak
    );
    assert!(
        shot.sharpness.is_finite() && (0.0..5.0).contains(&shot.sharpness),
        "{} sharpness {}",
        shot.id,
        shot.sharpness
    );
    assert!(
        shot.temporal_stability.is_finite() && (0.0..=1.0).contains(&shot.temporal_stability),
        "{} temporal stability {}",
        shot.id,
        shot.temporal_stability
    );
    assert!(
        shot.ground_luminance.is_finite() && (0.0..=255.0).contains(&shot.ground_luminance),
        "{} ground luminance {}",
        shot.id,
        shot.ground_luminance
    );
    assert_passes(&shot.gpu_ms, gpu_timing, &shot.id);
}

fn assert_passes(passes: &PassMillis, gpu_timing: bool, id: &str) {
    for value in [
        passes.shadow,
        passes.sky,
        passes.terrain,
        passes.scatter,
        passes.water,
        passes.total,
    ] {
        assert!(value.is_finite() && value >= 0.0, "{id} pass time {value}");
    }
    let sum = passes.shadow + passes.sky + passes.terrain + passes.scatter + passes.water;
    assert!(
        (sum - passes.total).abs() < 1.0e-3,
        "{id} pass sum {sum} vs total {}",
        passes.total
    );
    if !gpu_timing {
        assert_eq!(passes.total, 0.0, "{id} times should be zero without timestamps");
    }
}
