// SPDX-License-Identifier: MPL-2.0
//! The media resolver's paths go through the shared rule (`regia_core::project_path`, ADR-4922).
//! Its hand-written check treated `..\x`, `C:x` and `NUL` as plain names on Linux, which Windows
//! opens as the parent folder, a drive and a device.

use regia_media::{MediaError, MediaFiles};
use regia_test_utils::unique_temp_dir;

fn refused(files: &MediaFiles, path: &str) -> bool {
    matches!(files.open(path), Err(MediaError::SourceNotFound(_)))
}

#[test]
fn media_paths_stay_inside_the_project() {
    let top = unique_temp_dir("media-paths");
    let root = top.join("project");
    std::fs::create_dir_all(root.join("movies")).expect("dirs");
    std::fs::write(root.join("movies/intro.webm"), b"inside").expect("inside");
    std::fs::write(top.join("secret.webm"), b"outside").expect("outside");
    let mut files = MediaFiles::with_root(&root);
    files.insert("memory/clip.webm", b"memory".to_vec());
    // The controls: on disk, `\` as a separator, `.` segments, and an in-memory file.
    for ok in [
        "movies/intro.webm",
        "movies\\intro.webm",
        "./movies/intro.webm",
        "memory/clip.webm",
    ] {
        assert!(files.open(ok).is_ok(), "{ok}");
    }
    let absolute = top.join("secret.webm").display().to_string();
    for bad in [
        "",
        "../secret.webm",
        "movies/../../secret.webm",
        "..\\secret.webm",
        absolute.as_str(),
        "movies/missing.webm",
    ] {
        assert!(refused(&files, bad), "{bad:?}");
    }
    let _ = std::fs::remove_dir_all(&top);
}

#[cfg(unix)]
#[test]
fn media_paths_do_not_follow_a_link_out_of_the_project() {
    let top = unique_temp_dir("media-links");
    let root = top.join("project");
    let outside = top.join("outside");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.webm"), b"outside").expect("secret");
    std::os::unix::fs::symlink(&outside, root.join("linked")).expect("dir link");
    std::os::unix::fs::symlink(outside.join("secret.webm"), root.join("file.webm")).expect("file link");
    let files = MediaFiles::with_root(&root);
    assert!(refused(&files, "linked/secret.webm"));
    assert!(refused(&files, "file.webm"));
    // A link that stays inside is fine.
    std::fs::create_dir_all(root.join("real")).expect("real");
    std::fs::write(root.join("real/a.webm"), b"x").expect("a");
    std::os::unix::fs::symlink(root.join("real"), root.join("alias")).expect("inner link");
    assert!(files.open("alias/a.webm").is_ok());
    let _ = std::fs::remove_dir_all(&top);
}

/// The Windows forms as a table on every host: each one that is a plain file name on Linux is
/// planted in the project, so only the rule can refuse it.
#[test]
fn windows_path_forms_are_refused_on_every_host() {
    let root = unique_temp_dir("media-windows");
    let files = MediaFiles::with_root(&root);
    for form in [
        "C:intro.webm",
        "C:\\intro.webm",
        "\\\\server\\share\\intro.webm",
        "\\intro.webm",
        "NUL",
        "con.webm",
        "LPT1.webm",
        "intro.webm:alt",
        "...\\intro.webm",
    ] {
        let _ = std::fs::write(root.join(form), b"planted");
        assert!(refused(&files, form), "{form:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
