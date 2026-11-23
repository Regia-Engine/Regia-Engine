// SPDX-License-Identifier: MPL-2.0
//! Threat-model finding 27 (ADR-3206; fixed under ADR-4922): the state-tree include reader read
//! through a link and lacked the shared path rule. On Linux its hand-written check treated `..\x`,
//! `C:\x` and `NUL` as plain file names, which Windows opens as the parent folder, a drive and a
//! device.

use regia_ai::state_tree::read_state_tree_include;
use regia_test_utils::unique_temp_dir;

const TREE: &str = "(format_version: 2, root: \"Root\", states: [(name: \"Root\")])";

#[test]
fn an_include_inside_the_project_is_read() {
    let root = unique_temp_dir("st-include-ok");
    std::fs::create_dir_all(root.join("trees")).expect("dir");
    std::fs::write(root.join("trees/guard.statetree.ron"), TREE).expect("tree");
    assert_eq!(
        read_state_tree_include(&root, "trees/guard.statetree.ron").as_deref(),
        Some(TREE)
    );
    assert_eq!(
        read_state_tree_include(&root, "trees\\guard.statetree.ron").as_deref(),
        Some(TREE),
        "`\\` separates folders on every host"
    );
    assert_eq!(
        read_state_tree_include(&root, "trees/missing.statetree.ron"),
        None
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_include_cannot_climb_out_of_the_project() {
    let top = unique_temp_dir("st-include-climb");
    let root = top.join("project");
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::write(top.join("outside.statetree.ron"), TREE).expect("outside");
    let absolute = top.join("outside.statetree.ron").display().to_string();
    for include in [
        "../outside.statetree.ron",
        "trees/../../outside.statetree.ron",
        absolute.as_str(),
    ] {
        assert_eq!(read_state_tree_include(&root, include), None, "{include}");
    }
    let _ = std::fs::remove_dir_all(&top);
}

#[cfg(unix)]
#[test]
fn an_include_cannot_reach_out_through_a_symlink() {
    let top = unique_temp_dir("st-include-link");
    let root = top.join("project");
    let outside = top.join("outside");
    std::fs::create_dir_all(&root).expect("dir");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("x.statetree.ron"), TREE).expect("outside tree");
    std::os::unix::fs::symlink(&outside, root.join("linked")).expect("dir link");
    std::os::unix::fs::symlink(outside.join("x.statetree.ron"), root.join("file.statetree.ron"))
        .expect("file link");
    assert_eq!(read_state_tree_include(&root, "linked/x.statetree.ron"), None);
    assert_eq!(read_state_tree_include(&root, "file.statetree.ron"), None);
    // A link that stays inside the project is fine.
    std::fs::create_dir_all(root.join("real")).expect("real");
    std::fs::write(root.join("real/y.statetree.ron"), TREE).expect("inner tree");
    std::os::unix::fs::symlink(root.join("real"), root.join("alias")).expect("inner link");
    assert_eq!(
        read_state_tree_include(&root, "alias/y.statetree.ron").as_deref(),
        Some(TREE)
    );
    let _ = std::fs::remove_dir_all(&top);
}

/// The Windows path forms as a table, on every host. Each name that is a plain single file name on
/// Linux is planted in the project, so a check that treats it as a name (the old one) reads it;
/// Windows would open the parent folder, a drive, a share, a device or a stream instead.
#[test]
fn windows_path_forms_are_refused_on_every_host() {
    let root = unique_temp_dir("st-include-windows");
    let forms = [
        "..\\outside.statetree.ron",
        "a\\..\\..\\outside.statetree.ron",
        "C:\\outside.statetree.ron",
        "C:outside.statetree.ron",
        "\\\\server\\share\\x.statetree.ron",
        "\\outside.statetree.ron",
        "NUL",
        "con.statetree.ron",
        "COM1.statetree.ron",
        "x.statetree.ron:alt",
        "...\\x.statetree.ron",
        "x.statetree.ron\0",
    ];
    for form in forms {
        if !form.contains(['/', '\0']) {
            let _ = std::fs::write(root.join(form), TREE);
        }
        assert_eq!(read_state_tree_include(&root, form), None, "{form:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
