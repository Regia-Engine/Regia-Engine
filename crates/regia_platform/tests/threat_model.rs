// SPDX-License-Identifier: MPL-2.0
//! The scrubber as a crash report uses it (`docs/security/threat-model.md`, ADR-3206). `regia_core`
//! tests the scrubber; this tests that the report goes through it. Its own binary: it sets the panic
//! hook and the process-global crash folder.

use regia_platform::crash;

#[test]
fn a_crash_report_scrubs_the_log_lines_it_carries() {
    let dir = std::env::temp_dir().join(format!("regia-threat-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    crash::configure(
        Some(dir.clone()),
        crash::BuildInfo::detect(),
        regia_platform::OsInfo::detect(),
    );
    regia_core::log::ring::push("WARN", "game", "connecting with password=hunter2".to_string());
    std::panic::set_hook(Box::new(|info| {
        let _ = crash::write_crash_report(info);
    }));
    let caught = std::panic::catch_unwind(|| panic!("boom"));
    let _ = std::panic::take_hook();
    assert!(caught.is_err());
    let report = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("crash-"))
        .expect("a report was written");
    let text = std::fs::read_to_string(report.path()).expect("report");
    assert!(
        text.contains("connecting with"),
        "the line is in the report: {text}"
    );
    assert!(!text.contains("hunter2"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
