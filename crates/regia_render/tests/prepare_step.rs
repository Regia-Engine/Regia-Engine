// SPDX-License-Identifier: MPL-2.0
//! Install prepare, end to end, in its own process.
//!
//! `pipeline_cache::set_directory` and `pipeline_warm::release_deferred` are process-global,
//! so this is not a library unit test. The run uses the Boot thread budget, does not renice
//! the test runner, and a second call must skip.

use std::time::Duration;

use regia_render::boot_threads;
use regia_render::prepare::{self, PrepareRequest, PrepareState};

#[test]
fn prepare_warms_then_the_second_run_skips() {
    // The drivers' cache env and `REGIA_PIPELINE_CACHE_DIR` would send the stamp somewhere
    // else. Nothing else in this binary reads the environment concurrently.
    #[allow(unsafe_code)]
    // SAFETY: one test, before any warm worker or driver exists.
    unsafe {
        std::env::remove_var("REGIA_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_PIPELINE_CACHE_DIR");
        std::env::remove_var("REGIA_PIPELINE_CACHE");
        std::env::remove_var("FERRITE_PIPELINE_CACHE");
        std::env::remove_var("REGIA_SHIPPED_PIPELINE_CACHE_DIR");
        std::env::remove_var("FERRITE_SHIPPED_PIPELINE_CACHE_DIR");
    }
    let dir = std::env::temp_dir().join(format!("regia-prepare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let mut request = PrepareRequest::new(dir.clone());
    request.nice = false;
    request.timeout = Duration::from_secs(360);

    let mut lines = Vec::new();
    let report = prepare::run(&request, |progress| lines.push(progress.to_json()))
        .unwrap_or_else(|err| panic!("prepare failed: {err}"));
    assert!(!report.skipped, "the first run has no stamp");
    assert_eq!(
        report.threads,
        boot_threads(),
        "prepare must stay on the Boot budget (note_scene_frame would drop it to Play)"
    );
    assert!(report.units.contains("splash:bgra8-srgb"));
    assert!(report.units.contains("canonical:rgba8-srgb"));
    assert!(!report.units.contains("profile:"), "no profile was requested");
    let last = lines.last().expect("a progress line");
    assert!(last.starts_with('{') && last.ends_with('}'), "{last}");
    assert!(!last.contains('\n'));
    assert!(last.contains("\"state\":\"ready\""));
    assert!(last.contains("\"fraction\":1.0000"));
    assert!(last.contains("\"skipped\":false"));

    let status = prepare::read_status(&prepare::status_path(&dir))
        .expect("status")
        .expect("status file");
    assert_eq!(status.state, PrepareState::Ready);
    let stamp = prepare::read_stamp(&report.stamp_path)
        .expect("stamp")
        .expect("stamp file");
    assert!(stamp.ok);
    assert!(stamp.is_current(&stamp.key, &report.units));
    assert!(
        prepare::stamp_is_current(&request).expect("check"),
        "the stamp must match the device this run used"
    );

    let again = prepare::run(&request, |_| {}).unwrap_or_else(|err| panic!("second prepare failed: {err}"));
    assert!(again.skipped, "a current stamp skips the warm");
    assert_eq!(again.threads, boot_threads());

    let _ = std::fs::remove_dir_all(&dir);
}
