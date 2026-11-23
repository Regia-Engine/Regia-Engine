// SPDX-License-Identifier: MPL-2.0
//! A mod's file paths go through the shared rule (`regia_core::project_path`, ADR-4922) besides the
//! loader's own: names Windows opens as a device or reads as `..` are dropped on every host, a
//! `..` climb never enters the map, and a symlink out of the mod is not followed.

use regia_script_lua::ModFiles;
use regia_test_utils::unique_temp_dir;

#[test]
fn windows_names_and_climbs_are_dropped_from_a_mod_map() {
    let files = ModFiles::from_map([
        ("main.lua", "ok"),
        ("lib/util.lua", "ok"),
        ("nul.lua", "x"),
        ("lib/con.lua", "x"),
        ("com1/x.lua", "x"),
        ("lib/.../x.lua", "x"),
        ("lib/x.lua ", "x"),
        ("../escape.lua", "x"),
        ("lib/../../escape.lua", "x"),
        ("C:x.lua", "x"),
    ]);
    let kept: Vec<&str> = files.paths().collect();
    // `lib/x.lua ` is dropped too: Windows drops the trailing space and opens `lib/x.lua`, so the
    // name the check sees is not the file the open gets, and the shared rule refuses it on every
    // host (`path_trailing`, ADR-3206).
    assert_eq!(kept, ["lib/util.lua", "main.lua"]);
}

#[test]
fn a_mod_folder_with_device_names_is_read_without_them() {
    let dir = unique_temp_dir("lua-mod-devices");
    std::fs::create_dir_all(dir.join("lib")).expect("lib");
    std::fs::write(dir.join("main.lua"), "return 1").expect("main");
    std::fs::write(dir.join("lib/aux.lua"), "return 2").expect("aux");
    std::fs::write(dir.join("lib/prn.lua"), "return 3").expect("prn");
    std::fs::write(dir.join("lib/ok.lua"), "return 4").expect("ok");
    let files = ModFiles::from_dir(&dir, 1 << 20).expect("reads");
    assert_eq!(files.paths().collect::<Vec<_>>(), ["lib/ok.lua", "main.lua"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_mod_is_not_followed() {
    let top = unique_temp_dir("lua-mod-links");
    let dir = top.join("mod");
    let outside = top.join("outside");
    std::fs::create_dir_all(&dir).expect("mod");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.lua"), "return 'secret'").expect("secret");
    std::fs::write(dir.join("main.lua"), "return 1").expect("main");
    std::os::unix::fs::symlink(&outside, dir.join("lib")).expect("dir link");
    std::os::unix::fs::symlink(outside.join("secret.lua"), dir.join("s.lua")).expect("file link");
    let files = ModFiles::from_dir(&dir, 1 << 20).expect("reads");
    assert_eq!(files.paths().collect::<Vec<_>>(), ["main.lua"]);
    let _ = std::fs::remove_dir_all(&top);
}
