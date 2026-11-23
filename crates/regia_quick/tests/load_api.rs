// SPDX-License-Identifier: MPL-2.0
//! `BytesRequest` and `Quick::load_bytes` / `Quick::vfs_path`: path parsing, the one-shot poll
//! contract and the error paths. Reads are waited on with `wait()` (no sleeps, no clocks).

use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_platform::{DirMount, MountPoint, Vfs};
use regia_quick::{BytesRequest, Quick};
use regia_test_utils::unique_temp_dir;

fn vfs_with(files: &[(&str, Vec<u8>)]) -> (Vfs, std::path::PathBuf) {
    let dir = unique_temp_dir("quick-load-api");
    for (name, bytes) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, bytes).expect("write fixture");
    }
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Assets, DirMount::read_only(&dir));
    vfs.mount(MountPoint::User, DirMount::writable(&dir));
    (vfs, dir)
}

#[test]
fn bare_paths_mean_assets_and_explicit_mounts_are_kept() {
    let bare = Quick::vfs_path("levels/a.ron").expect("bare path");
    assert_eq!(bare.mount(), MountPoint::Assets);
    assert_eq!(bare.relative(), "levels/a.ron");
    let user = Quick::vfs_path("user://saves/slot1.bin").expect("user path");
    assert_eq!(user.mount(), MountPoint::User);
    assert_eq!(user.relative(), "saves/slot1.bin");
    let root = Quick::vfs_path("cache://").expect("mount root");
    assert!(root.is_root());
    assert_eq!(root.mount(), MountPoint::Cache);
    // `.` segments are dropped like any VfsPath.
    assert_eq!(
        Quick::vfs_path("./a/./b").expect("dot segments").relative(),
        "a/b"
    );
}

#[test]
fn invalid_paths_are_errors_that_name_the_path() {
    for bad in [
        "../escape",
        "a/../b",
        "/absolute",
        "a//b",
        "a\\b",
        "bogus://x",
        "assets://a:b",
    ] {
        let err = Quick::vfs_path(bad).expect_err(bad);
        assert!(err.starts_with(bad), "the error names the input: {err}");
    }
}

#[test]
fn a_failed_request_reports_once_then_goes_quiet() {
    let mut request = BytesRequest::failed("x.bin", "boom".into());
    assert_eq!(request.path(), "x.bin");
    assert_eq!(request.poll().expect("ready at once"), Err("boom".to_string()));
    assert!(request.poll().is_none(), "the result is handed out once");
    assert!(
        request
            .wait()
            .expect_err("nothing left")
            .contains("already taken"),
        "wait after poll says the result was taken"
    );
    assert_eq!(
        BytesRequest::failed("y.bin", "no".into())
            .wait()
            .expect_err("fails"),
        "no"
    );
}

#[test]
fn start_with_a_bad_path_fails_without_touching_the_io_pool() {
    let (vfs, dir) = vfs_with(&[]);
    let mut request = BytesRequest::start(&vfs, "../../etc/passwd");
    assert_eq!(request.path(), "../../etc/passwd");
    let err = request
        .poll()
        .expect("fails immediately")
        .expect_err("invalid path");
    assert!(err.contains("../../etc/passwd"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reads_succeed_on_every_readable_mount_and_fail_for_missing_or_unmounted() {
    regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
    let (vfs, dir) = vfs_with(&[("data/blob.bin", vec![1, 2, 3, 4]), ("empty.bin", Vec::new())]);
    let nested = BytesRequest::start(&vfs, "data/blob.bin")
        .wait()
        .expect("nested read");
    assert_eq!(nested.as_ref(), [1, 2, 3, 4]);
    let user = BytesRequest::start(&vfs, "user://data/blob.bin")
        .wait()
        .expect("user read");
    assert_eq!(user.as_ref(), [1, 2, 3, 4]);
    let empty = BytesRequest::start(&vfs, "empty.bin").wait().expect("empty file");
    assert!(empty.is_empty(), "an empty file is a success, not an error");
    assert!(BytesRequest::start(&vfs, "data/missing.bin").wait().is_err());
    assert!(
        BytesRequest::start(&vfs, "config://anything").wait().is_err(),
        "config:// is not mounted in this Vfs"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The load outcomes a test system records.
type Outcomes = Arc<Mutex<Vec<Result<Vec<u8>, String>>>>;

#[test]
fn quick_load_bytes_uses_the_vfs_resource_or_reports_that_it_is_missing() {
    regia_core::tasks::init(regia_core::tasks::TaskPoolOptions::default());
    let result: Outcomes = Arc::default();

    // Without a Vfs the request is already failed.
    let sink = result.clone();
    let mut app = App::new();
    app.add_systems(Update, move |q: Quick| {
        let outcome = q.load_bytes("a.bin").wait().map(|b| b.to_vec());
        sink.lock().expect("lock").push(outcome);
    });
    app.update();

    // With one it reads the file.
    let (vfs, dir) = vfs_with(&[("a.bin", b"quick".to_vec())]);
    let sink = result.clone();
    let mut app = App::new();
    app.insert_resource(vfs);
    app.add_systems(Update, move |q: Quick| {
        let outcome = q.load_bytes("a.bin").wait().map(|b| b.to_vec());
        sink.lock().expect("lock").push(outcome);
    });
    app.update();

    let got = result.lock().expect("lock");
    assert_eq!(got.len(), 2);
    assert!(
        got[0].as_ref().is_err_and(|e| e.contains("no Vfs")),
        "{:?}",
        got[0]
    );
    assert_eq!(got[1], Ok(b"quick".to_vec()));
    let _ = std::fs::remove_dir_all(dir);
}
