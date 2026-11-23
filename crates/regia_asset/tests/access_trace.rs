// SPDX-License-Identifier: MPL-2.0
//! Asset I/O tracing is exercised in child processes so tests never mutate global environment.

use std::process::Command;

#[global_allocator]
static ALLOCATOR: regia_test_utils::alloc_counter::CountingAlloc =
    regia_test_utils::alloc_counter::CountingAlloc::new();

use regia_asset::{AssetId, AssetServer, AssetServerSettings, cooked_cache_dir, cooked_file_name};

#[test]
fn trace_lists_a_missed_open() {
    let root = regia_test_utils::unique_temp_dir("asset-access-trace-miss");
    let log = root.join("access.log");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "trace_probe", "--ignored", "--nocapture"])
        .env("REGIA_ASSET_TRACE", &log)
        .env("REGIA_TRACE_TEST_ROOT", &root)
        .output()
        .expect("child starts");
    assert!(output.status.success(), "{output:?}");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let missing = cooked_cache_dir(&root, "linux").join(cooked_file_name(AssetId(regia_core::id::Guid(42))));
    let expected = format!("miss open {:?}", missing.to_string_lossy());
    assert!(
        text.lines().any(|line| line == expected),
        "missing trace row {expected:?}; got {text:?}"
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
#[ignore = "child process fixture; the parent supplies an isolated environment"]
fn trace_probe() {
    let root = std::path::PathBuf::from(std::env::var_os("REGIA_TRACE_TEST_ROOT").expect("fixture root"));
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root,
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server opens without a manifest");
    assert!(server.read_cooked(AssetId(regia_core::id::Guid(42))).is_err());
}

fn probe(mode: &str, sink: Option<&str>) -> (std::path::PathBuf, serde_json::Value) {
    let root = regia_test_utils::unique_temp_dir("asset-access-trace-cases");
    if mode == "append" {
        regia_test_utils::write_atomic(&root.join("access.log"), b"prior session\n").expect("prior log");
    }
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .args(["--exact", "trace_cases", "--ignored", "--nocapture"])
        .env("REGIA_TRACE_TEST_ROOT", &root)
        .env("REGIA_TRACE_TEST_MODE", mode)
        .env_remove("REGIA_ASSET_TRACE");
    if let Some(sink) = sink {
        child.env(
            "REGIA_ASSET_TRACE",
            match sink {
                "file" => root.join("access.log"),
                "directory" => root.clone(),
                other => other.into(),
            },
        );
    }
    let output = child.output().expect("child starts");
    assert!(output.status.success(), "{output:?}");
    let snapshot =
        serde_json::from_slice(&std::fs::read(root.join("snapshot.json")).expect("snapshot")).expect("json");
    (root, snapshot)
}

#[test]
fn disabled_and_empty_trace_have_no_rows_or_file() {
    for sink in [None, Some("")] {
        let (root, snapshot) = probe("reads", sink);
        assert_eq!(snapshot["enabled"], false);
        assert_eq!(snapshot["total"], 0);
        assert!(!root.join("access.log").exists());
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[test]
fn trace_preserves_success_and_distinguishes_miss_from_error() {
    let (root, snapshot) = probe("reads", Some("file"));
    assert_eq!(snapshot["written"], 3);
    let rows = snapshot["events"].as_array().expect("rows");
    assert_eq!(
        rows.iter()
            .map(|r| r["outcome"].as_str().expect("outcome"))
            .collect::<Vec<_>>(),
        ["ok", "miss", "err"]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("access.log"))
            .expect("log")
            .lines()
            .count(),
        3
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn unavailable_sink_does_not_break_asset_reads() {
    let (root, snapshot) = probe("reads", Some("directory"));
    assert_eq!(snapshot["enabled"], true);
    assert_eq!(snapshot["sink_available"], false);
    assert_eq!(snapshot["sink_error"], "asset_trace_open");
    assert_eq!(snapshot["total"], 3);
    assert_eq!(snapshot["written"], 0);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(target_os = "linux")]
#[test]
fn failed_sink_write_is_reported_without_breaking_reads() {
    let (root, snapshot) = probe("reads", Some("/dev/full"));
    assert_eq!(snapshot["sink_error"], "asset_trace_write");
    assert_eq!(snapshot["sink_available"], false);
    assert_eq!(snapshot["total"], 3);
    assert_eq!(snapshot["written"], 0);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn concurrent_payload_reads_keep_complete_rows_and_bounded_history() {
    let (root, snapshot) = probe("parallel", Some("file"));
    assert_eq!(snapshot["total"], 400);
    assert_eq!(snapshot["written"], 400);
    assert_eq!(snapshot["evicted"], 144);
    let rows = snapshot["events"].as_array().expect("rows");
    assert_eq!(rows.len(), 256);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["sequence"], (145 + i) as u64);
        assert_eq!(row["outcome"], "ok");
    }
    let text = std::fs::read_to_string(root.join("access.log")).expect("log");
    assert_eq!(text.lines().count(), 400);
    assert!(
        text.lines()
            .all(|s| s.starts_with("ok open \"") && s.ends_with('"'))
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn logical_paths_escape_newlines_and_bound_recent_bytes() {
    let (root, snapshot) = probe("paths", Some("file"));
    let text = std::fs::read_to_string(root.join("access.log")).expect("log");
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("\\n"));
    let rows = snapshot["events"].as_array().expect("rows");
    assert_eq!(rows[0]["operation"], "resolve");
    assert_eq!(rows[0]["outcome"], "miss");
    assert_eq!(rows[1]["path_truncated"], true);
    assert!(rows[1]["path"].as_str().expect("path").len() <= regia_asset::access_trace::RECENT_PATH_BYTES);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
#[ignore = "child process fixture; the parent supplies an isolated environment"]
fn trace_cases() {
    let root = std::path::PathBuf::from(std::env::var_os("REGIA_TRACE_TEST_ROOT").expect("root"));
    let mut server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".into(),
        content_dir: None,
    })
    .expect("server");
    let id = AssetId(regia_core::id::Guid(7));
    let cache = cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache");
    regia_test_utils::write_atomic(&cache.join(cooked_file_name(id)), b"payload").expect("fixture");
    match std::env::var("REGIA_TRACE_TEST_MODE").expect("mode").as_str() {
        "reads" | "append" => {
            assert_eq!(server.read_cooked(id).expect("read").0, b"payload");
            assert!(server.read_cooked(AssetId(regia_core::id::Guid(8))).is_err());
            let directory_id = AssetId(regia_core::id::Guid(9));
            std::fs::create_dir(cache.join(cooked_file_name(directory_id))).expect("directory");
            assert!(server.read_cooked(directory_id).is_err());
        }
        "parallel" => {
            let reader = server.payload_reader();
            std::thread::scope(|scope| {
                for _ in 0..8 {
                    let reader = &reader;
                    scope.spawn(move || {
                        for _ in 0..50 {
                            assert_eq!(&*reader.read(id).expect("payload").0, b"payload");
                        }
                    });
                }
            });
        }
        "pak" => {
            use regia_asset::{FPAK_VERSION, PakEntry, PakReader, PakToc, encode_pak_header};
            let missing = AssetId(regia_core::id::Guid(99));
            assert!(PakReader::open(&root.join("missing.fpak")).is_err());
            let non_directory = root.join("regular-file");
            regia_test_utils::write_atomic(&non_directory, b"data").expect("regular file");
            let error = PakReader::open(&non_directory.join("missing.fpak"))
                .err()
                .expect("not a directory");
            assert!(matches!(error, regia_asset::PakError::Io(_)));
            let offset = regia_asset::FPAK_BLOB_ALIGNMENT;
            let mut data = vec![0u8; offset as usize];
            data.extend_from_slice(b"pak data");
            let toc_offset = data.len() as u64;
            let toc = PakToc {
                entries: vec![PakEntry {
                    id,
                    type_tag: regia_core::id::TypePathHash::of("test::Blob"),
                    offset,
                    size: 8,
                    compressed_size: 8,
                    block_table_offset: 0,
                }],
                manifest: Vec::new(),
                dependencies: Vec::new(),
            };
            let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
            data.extend_from_slice(&toc_bytes);
            let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
            data[..60].copy_from_slice(&encode_pak_header(
                FPAK_VERSION,
                0,
                toc_offset,
                toc_bytes.len() as u64,
                &hash,
            ));
            let pak_path = root.join("base.fpak");
            regia_test_utils::write_atomic(&pak_path, &data).expect("pak file");
            let pak = std::sync::Arc::new(PakReader::open(&pak_path).expect("pak"));
            assert_eq!(&*pak.read_shared(id).expect("shared"), b"pak data");
            assert_eq!(pak.read_decompressed(id).expect("decompressed"), b"pak data");
            assert_eq!(&*pak.read_raw(id).expect("raw"), b"pak data");
            assert!(pak.read_shared(missing).is_err());
            assert!(pak.read_raw(missing).is_err());
            assert!(pak.read_decompressed(missing).is_err());
        }
        "disabled_allocations" => {
            assert!(!regia_asset::access_trace::snapshot(0).enabled);
            assert!(server.id_for_path("assets/missing.png").is_none());
            let (_, measured) = regia_test_utils::alloc_counter::measure(|| {
                for _ in 0..1000 {
                    assert!(server.id_for_path("assets/missing.png").is_none());
                }
            });
            assert_eq!(measured.allocs, 0, "disabled tracing must not format paths");
        }
        "paths" => {
            assert!(server.load_mesh_path("assets/a\n\"b\\c").is_err());
            assert!(server.load_mesh_path(&"界".repeat(2000)).is_err());
        }
        other => panic!("unexpected mode {other}"),
    }
    let snapshot = regia_asset::access_trace::snapshot(usize::MAX);
    assert!(regia_asset::access_trace::snapshot(0).events.is_empty());
    let bytes = serde_json::to_vec(&snapshot).expect("serialize");
    regia_test_utils::write_atomic(&root.join("snapshot.json"), &bytes).expect("snapshot");
}

#[test]
fn trace_appends_to_an_existing_log() {
    let (root, snapshot) = probe("append", Some("file"));
    let text = std::fs::read_to_string(root.join("access.log")).expect("log");
    assert!(text.starts_with("prior session\n"));
    assert_eq!(text.lines().count(), 4);
    assert_eq!(snapshot["written"], 3);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn pak_opens_and_all_payload_entry_points_are_traced_once() {
    let (root, snapshot) = probe("pak", Some("file"));
    assert_eq!(snapshot["written"], 9);
    let rows = snapshot["events"].as_array().expect("events");
    assert_eq!(rows[0]["operation"], "open");
    assert_eq!(rows[0]["outcome"], "miss");
    assert_eq!(rows[1]["operation"], "open");
    assert_eq!(rows[1]["outcome"], "miss");
    assert_eq!(rows[2]["operation"], "open");
    assert_eq!(rows[2]["outcome"], "ok");
    for (i, row) in rows[3..].iter().enumerate() {
        assert_eq!(row["operation"], "pak");
        assert_eq!(row["outcome"], if i < 3 { "ok" } else { "miss" });
        assert!(row["path"].as_str().expect("path").contains("base.fpak#"));
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn disabled_trace_adds_no_allocations_to_warm_lookups() {
    let (root, _) = probe("disabled_allocations", None);
    std::fs::remove_dir_all(root).expect("cleanup");
}
