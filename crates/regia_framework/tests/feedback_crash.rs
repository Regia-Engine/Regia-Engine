// SPDX-License-Identifier: MPL-2.0
//! The automatic crash bundle: the panic hook writes a bundle with every part except the
//! screenshot, and the next launch tells the player once where it is.
//!
//! Its own test binary: the panic hook is process-wide.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::feedback::{
    Feedback, FeedbackCategory, FeedbackConfig, FeedbackHost, FeedbackPlugin, list_bundles, read_bundle,
};
use regia_framework::settings::GameSettings;

fn app(dir: &std::path::Path) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            dir: Some(dir.to_path_buf()),
            ..FeedbackConfig::default()
        },
    });
    app.insert_resource(GameSettings::default());
    app
}

fn text(entries: &[regia_framework::feedback::zip::ZipEntry], name: &str) -> String {
    let e = entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("missing {name}"));
    String::from_utf8(e.data.clone()).expect("utf8")
}

#[test]
fn panic_hook_writes_a_crash_bundle_and_the_next_launch_reports_it() {
    let dir = std::env::temp_dir().join(format!("regia-feedback-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut first = app(&dir);
    first
        .world_mut()
        .resource_mut::<FeedbackHost>()
        .set("match_id", "m-crash");
    for _ in 0..30 {
        first.update();
    }
    let caught = std::panic::catch_unwind(|| panic!("feedback crash test: boom"));
    assert!(caught.is_err());

    let bundles = list_bundles(&dir);
    assert_eq!(bundles.len(), 1, "{bundles:?}");
    assert_eq!(bundles[0].category, Some(FeedbackCategory::Crash));
    let entries = read_bundle(&bundles[0].path).expect("readable");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for part in [
        "feedback.json",
        "note.txt",
        "frame_times.csv",
        "hitch_report.txt",
        "system.txt",
        "settings.ron",
        "render_problems.txt",
        "log_tail.txt",
        "build_info.json",
        "host.json",
        "crash.txt",
    ] {
        assert!(names.contains(&part), "missing {part} in {names:?}");
    }
    assert!(
        !names.contains(&"screenshot.png"),
        "no screenshot in a crash bundle"
    );
    let crash = text(&entries, "crash.txt");
    assert!(crash.contains("panic: feedback crash test: boom"), "{crash}");
    assert!(crash.contains("feedback_crash.rs"), "{crash}");
    let meta: serde_json::Value = serde_json::from_str(&text(&entries, "feedback.json")).expect("json");
    assert_eq!(meta["category"], "crash");
    assert!(
        meta["screenshot_missing"]
            .as_str()
            .is_some_and(|s| s.contains("crash"))
    );
    assert!(meta["frames"].as_u64().is_some_and(|n| n >= 20), "{meta}");
    assert!(text(&entries, "host.json").contains("m-crash"));
    assert!(text(&entries, "settings.ron").contains("preset"));
    assert!(text(&entries, "frame_times.csv").contains(",16.000,"));

    // A second panic right after is rate-limited (a caught panic in a loop must not fill the disk).
    let _ = std::panic::catch_unwind(|| panic!("again"));
    assert_eq!(list_bundles(&dir).len(), 1);
    drop(first);

    // Next launch: the player is told once.
    let mut second = app(&dir);
    second.update();
    let notice = second.world().resource::<Feedback>().crash_notice().to_vec();
    assert_eq!(notice.len(), 1);
    assert_eq!(notice[0].name, bundles[0].name);
    drop(second);
    let mut third = app(&dir);
    third.update();
    assert!(third.world().resource::<Feedback>().crash_notice().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
