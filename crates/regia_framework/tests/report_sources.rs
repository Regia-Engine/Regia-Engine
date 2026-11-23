// SPDX-License-Identifier: MPL-2.0
//! Public artifact discovery uses the mounted log directory, not inferred OS paths.

#![cfg(feature = "client")]

use regia_app::App;
use regia_framework::report_sources::{ReportSourceDirs, ReportSourceKind, ReportSourceOptions};
use regia_platform::{DirMount, MountPoint, Vfs};
use regia_test_utils::{TempDir, write_atomic};

#[test]
fn report_sources_lists_logs_dir() {
    let dir = TempDir::new("report-sources-logs");
    write_atomic(&dir.join("2026-11-16_12-00-00.log"), b"session log\n").expect("seed log");
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("2026-11-16_12-00-00.log"))
        .expect("log metadata handle")
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("fixed modification time");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Logs, DirMount::writable(dir.path()));
    let mut app = App::new();
    app.insert_resource(vfs);
    let dirs = ReportSourceDirs::from_world(app.world());
    assert_eq!(dirs.logs.as_deref(), Some(dir.path()));
    let report = ReportSourceDirs {
        logs: dirs.logs,
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    let log = report
        .artifacts
        .iter()
        .find(|a| a.kind == ReportSourceKind::Log)
        .expect("log artifact");
    assert_eq!(log.path, dir.join("2026-11-16_12-00-00.log"));
    assert_eq!(log.bytes, 12);
    assert_eq!(log.modified_unix_ms, 1_600_000_000_000);
}

fn cache_key() -> regia_render::pipeline_cache::PipelineCacheKey {
    regia_render::pipeline_cache::PipelineCacheKey {
        backend: "Vulkan".into(),
        vendor: 7,
        device: 9,
        driver: "test-driver".into(),
        driver_info: "test-version".into(),
        wgpu_version: "test-wgpu".into(),
        shader_hash: "test-shaders".into(),
    }
}

#[test]
fn report_sources_lists_all_kinds_and_parses_cache_header() {
    let dir = TempDir::new("report-sources-all");
    for name in [
        "b.log",
        "a.jsonl",
        "run.summary.json",
        "run.summary.partial.json",
        "summary.json",
        "summary.partial.json",
        "crash-2026-11-16.txt",
        "native.crash.bin",
        "feedback-run-bug.zip",
    ] {
        write_atomic(&dir.join(name), b"fixture").expect("fixture");
    }
    for name in [
        "settings.ron",
        "crash-old.rejected",
        "feedback-run.zip.tmp",
        "pipelines-x.fpc.tmp",
    ] {
        write_atomic(&dir.join(name), b"skip").expect("ignored file");
    }
    std::fs::create_dir(dir.join("directory.log")).expect("directory");
    let key = cache_key();
    write_atomic(
        &dir.join("pipelines-test.fpc"),
        &regia_render::pipeline_cache::encode(&key, b"payload"),
    )
    .expect("cache");
    let sources = ReportSourceDirs {
        logs: Some(dir.to_path_buf()),
        crashes: Some(dir.to_path_buf()),
        feedback: Some(dir.to_path_buf()),
        pipeline_cache: Some(dir.to_path_buf()),
    }
    .list(&ReportSourceOptions::default());
    assert!(sources.issues.is_empty(), "{:?}", sources.issues);
    assert!(!sources.truncated);
    assert_eq!(sources.artifacts.len(), 10, "duplicate roots deduplicate files");
    assert!(
        sources
            .artifacts
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    for (kind, count) in [
        (ReportSourceKind::Log, 6),
        (ReportSourceKind::Crash, 2),
        (ReportSourceKind::Feedback, 1),
        (ReportSourceKind::PipelineCache, 1),
    ] {
        assert_eq!(sources.artifacts.iter().filter(|a| a.kind == kind).count(), count);
    }
    let cache = sources
        .artifacts
        .iter()
        .find(|a| a.kind == ReportSourceKind::PipelineCache)
        .expect("cache");
    assert_eq!(cache.pipeline_key.as_ref(), Some(&key));
}

#[test]
fn report_sources_missing_roots_are_empty_and_read_errors_are_visible() {
    let dir = TempDir::new("report-sources-missing");
    let missing = ReportSourceDirs {
        logs: Some(dir.join("absent")),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(missing.artifacts.is_empty());
    assert!(missing.issues.is_empty());
    assert!(!dir.join("absent").exists());
    write_atomic(&dir.join("file"), b"not a directory").expect("file");
    let bad = ReportSourceDirs {
        logs: Some(dir.join("file")),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert_eq!(bad.issues.len(), 1);
    assert_eq!(bad.issues[0].code, "read_directory");
}

#[test]
fn report_sources_bounded_headers_reject_corruption_without_hiding_artifacts() {
    let dir = TempDir::new("report-sources-headers");
    let key = cache_key();
    let valid = regia_render::pipeline_cache::encode(&key, b"payload");
    let mut version = valid.clone();
    version[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut huge = valid.clone();
    huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    for (name, bytes) in [
        ("short", b"FPCH".as_slice()),
        ("magic", b"not a cache!xxx"),
        ("version", version.as_slice()),
        ("length", huge.as_slice()),
    ] {
        write_atomic(&dir.join(format!("pipelines-{name}.fpc")), bytes).expect("bad cache");
    }
    let dirs = ReportSourceDirs {
        pipeline_cache: Some(dir.to_path_buf()),
        ..Default::default()
    };
    let report = dirs.list(&ReportSourceOptions::default());
    assert_eq!(report.artifacts.len(), 4);
    assert_eq!(report.issues.len(), 4);
    assert!(report.artifacts.iter().all(|a| a.pipeline_key.is_none()));
    assert!(report.issues.iter().all(|issue| issue.code == "invalid_header"));
    write_atomic(&dir.join("pipelines-valid.fpc"), &valid).expect("valid cache");
    let bounded = dirs.list(&ReportSourceOptions {
        max_header_bytes: 12,
        ..Default::default()
    });
    assert_eq!(
        bounded.issues.len(),
        5,
        "a key outside the caller's budget is not read"
    );
    let zero = dirs.list(&ReportSourceOptions {
        max_header_bytes: 0,
        ..Default::default()
    });
    assert_eq!(zero.issues.len(), 5);
}

#[test]
fn report_sources_header_inspection_does_not_claim_payload_validation() {
    let dir = TempDir::new("report-sources-prefix");
    let key = cache_key();
    let mut bytes = regia_render::pipeline_cache::encode(&key, b"payload");
    bytes.truncate(12 + key.canonical().len());
    write_atomic(&dir.join("pipelines-prefix.fpc"), &bytes).expect("header only");
    let sources = ReportSourceDirs {
        pipeline_cache: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(sources.issues.is_empty());
    assert_eq!(sources.artifacts[0].pipeline_key, Some(key));
}

#[test]
fn report_sources_directory_budget_marks_incomplete_inventory() {
    let dir = TempDir::new("report-sources-limit");
    write_atomic(&dir.join("a.log"), b"one").expect("one");
    write_atomic(&dir.join("b.log"), b"two").expect("two");
    let dirs = ReportSourceDirs {
        logs: Some(dir.to_path_buf()),
        ..Default::default()
    };
    for limit in [0, 1, 2] {
        let report = dirs.list(&ReportSourceOptions {
            max_entries_per_directory: limit,
            ..Default::default()
        });
        assert_eq!(report.artifacts.len(), limit);
        assert_eq!(report.truncated, limit < 2);
    }
}

#[cfg(unix)]
#[test]
fn report_sources_skips_symlinks_and_non_regular_files() {
    let dir = TempDir::new("report-sources-links");
    let outside = TempDir::new("report-sources-outside");
    write_atomic(&outside.join("private"), b"private data").expect("outside");
    std::os::unix::fs::symlink(outside.join("private"), dir.join("pipelines-link.fpc")).expect("link");
    std::os::unix::fs::symlink(outside.join("missing"), dir.join("pipelines-broken.fpc")).expect("broken");
    std::fs::create_dir(dir.join("pipelines-dir.fpc")).expect("directory");
    let sources = ReportSourceDirs {
        pipeline_cache: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(sources.artifacts.is_empty());
    assert!(sources.issues.is_empty());
}

#[test]
fn report_sources_feedback_uses_host_override_without_a_plugin() {
    let dir = TempDir::new("report-sources-feedback");
    let mut app = App::new();
    app.insert_resource(regia_framework::feedback::FeedbackConfig {
        dir: Some(dir.to_path_buf()),
        ..Default::default()
    });
    assert_eq!(
        ReportSourceDirs::from_world(app.world()).feedback.as_deref(),
        Some(dir.path())
    );
}

// Apple file systems (APFS, HFS+) refuse a file name that is not UTF-8 (EILSEQ), so neither the fixture nor
// such a log can exist there.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn report_sources_keeps_native_paths_with_non_utf8_stems() {
    use std::os::unix::ffi::OsStringExt;
    let dir = TempDir::new("report-sources-native-path");
    let path = dir.join(std::ffi::OsString::from_vec(b"session-\xff.log".to_vec()));
    write_atomic(&path, b"log").expect("native path");
    let report = ReportSourceDirs {
        logs: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(report.issues.is_empty());
    assert_eq!(report.artifacts.len(), 1);
    assert_eq!(report.artifacts[0].path, path);
}

#[cfg(unix)]
#[test]
fn report_sources_large_sparse_cache_reads_only_a_bounded_prefix() {
    let dir = TempDir::new("report-sources-sparse");
    let path = dir.join("pipelines-large.fpc");
    let key = cache_key();
    let bytes = regia_render::pipeline_cache::encode(&key, b"payload");
    write_atomic(&path, &bytes).expect("cache");
    let file = std::fs::OpenOptions::new().write(true).open(&path).expect("open");
    let len = 1024 * 1024 * 1024;
    file.set_len(len).expect("sparse file");
    drop(file);
    let report = ReportSourceDirs {
        pipeline_cache: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions {
        max_header_bytes: 12 + key.canonical().len(),
        ..Default::default()
    });
    assert!(report.issues.is_empty());
    assert_eq!(report.artifacts[0].bytes, len);
    assert_eq!(report.artifacts[0].pipeline_key, Some(key));
}

#[test]
fn report_sources_discovers_the_real_session_log_sink() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;
    let dir = TempDir::new("report-sources-file-sink");
    let sink = regia_core::log::FileSink::create(dir.path(), 2).expect("session sink");
    sink.make_writer()
        .write_all(b"real session record\n")
        .expect("record");
    sink.flush();
    let report = ReportSourceDirs {
        logs: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(report.issues.is_empty());
    assert_eq!(report.artifacts.len(), 1);
    assert_eq!(report.artifacts[0].kind, ReportSourceKind::Log);
    assert_eq!(report.artifacts[0].path, sink.path());
    assert_eq!(report.artifacts[0].bytes, b"real session record\n".len() as u64);
}

#[test]
fn report_sources_lists_preserved_native_dump_in_feedback_dir() {
    let dir = TempDir::new("report-sources-native-fallback");
    let dump = dir.join("native-crash-1600000000000.dmp");
    write_atomic(&dump, b"preserved dump").expect("fallback dump");
    write_atomic(&dir.join(".native-crash-123.dmp"), b"in progress").expect("temporary dump");
    write_atomic(&dir.join("unrelated.dmp"), b"unrelated").expect("unrelated dump");
    let report = ReportSourceDirs {
        feedback: Some(dir.to_path_buf()),
        ..Default::default()
    }
    .list(&ReportSourceOptions::default());
    assert!(report.issues.is_empty());
    assert_eq!(
        report.artifacts.len(),
        1,
        "preserved fallback must remain discoverable"
    );
    assert_eq!(report.artifacts[0].kind, ReportSourceKind::Crash);
    assert_eq!(report.artifacts[0].path, dump);
    assert_eq!(report.artifacts[0].bytes, b"preserved dump".len() as u64);
}
