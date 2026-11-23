// SPDX-License-Identifier: MPL-2.0
//! ADR-2307: a device-lost crash report is a pending crash report like a panic's, and every report
//! carries the registered sections (the renderer's `gpu breadcrumbs:` block), so the reporter
//! (`regia_crash`) uploads a report that names the passes the GPU was most likely inside.
//!
//! Its own test binary: the crash directory and the section list are process-wide.

fn trail() -> String {
    "gpu breadcrumbs: level 3 (timestamped), submitted 9, completed 8, acknowledged frame 4\n  \
     suspects (2): ssao, opaque\n"
        .to_owned()
}

fn empty() -> String {
    String::new()
}

#[test]
fn device_lost_report_is_pending_and_carries_the_sections() {
    // One test in this binary, so the pid alone makes the directory unique.
    let dir = std::env::temp_dir().join(format!("regia-crash-sections-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    regia_platform::crash::configure(
        Some(dir.clone()),
        regia_platform::crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    regia_platform::crash::add_report_section("gpu_breadcrumbs", trail);
    // An empty section adds nothing; a second registration of a name replaces the first.
    regia_platform::crash::add_report_section("empty", empty);
    regia_platform::crash::add_report_section("gpu_breadcrumbs", trail);
    assert_eq!(
        regia_platform::crash::report_sections(),
        vec!["empty", "gpu_breadcrumbs"]
    );

    let path = regia_platform::crash::write_device_lost_report("device_lost", "unknown", "hung in ssao", 3)
        .expect("written");
    let pending = regia_platform::crash::pending();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].path, path);
    let text = regia_platform::crash::read(&pending[0].name).expect("readable");
    for needle in [
        "device_lost: unknown: hung in ssao",
        "location: regia_render::gpu_health",
        "no backtrace: device_lost",
        "exits with code 3",
        "gpu breadcrumbs: level 3 (timestamped)",
        "suspects (2): ssao, opaque",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
    assert_eq!(
        text.matches("gpu breadcrumbs:").count(),
        1,
        "one block per section name"
    );
    assert!(!text.contains("panic:"), "a device loss is not a panic");
    let _ = std::fs::remove_dir_all(&dir);
}
