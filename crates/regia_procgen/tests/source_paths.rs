// SPDX-License-Identifier: MPL-2.0
//! `ProcGraphSource::Dir` reads stay inside the project (ADR-1816): absolute, drive, UNC, `..`
//! and symlinked paths read nothing, so generators, `graph.call`, variants and `in.table` cannot
//! name a file outside the project.

use regia_procgen::world::ProcGraphSource;

#[test]
fn dir_source_reads_only_inside_the_project() {
    let base = regia_test_utils::unique_temp_dir("procgen-source-paths");
    let root = base.join("project");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join("g")).expect("root");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(root.join("g/in.pgraph.ron"), "inside").expect("inside");
    std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
    let source = ProcGraphSource::Dir(root.clone());

    assert_eq!(source.read("g/in.pgraph.ron").as_deref(), Some("inside"));
    let abs = outside.join("secret.txt").display().to_string();
    for p in [
        abs.as_str(),
        "/etc/hostname",
        "../outside/secret.txt",
        "g/../../outside/secret.txt",
        "C:\\Windows\\win.ini",
        "C:secret.txt",
        "\\\\server\\share\\secret.txt",
        "//server/share/secret.txt",
        "\\\\?\\C:\\secret.txt",
        "g/in.pgraph.ron\0",
        "",
    ] {
        assert_eq!(source.read(p), None, "{p:?}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("out")).expect("dir link");
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("s.txt")).expect("file link");
        assert_eq!(source.read("out/secret.txt"), None);
        assert_eq!(source.read("s.txt"), None);
    }
    let _ = std::fs::remove_dir_all(&base);
}
