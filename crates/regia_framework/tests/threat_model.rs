// SPDX-License-Identifier: MPL-2.0
//! Mitigations named by `docs/security/threat-model.md` that had no test of their own (ADR-3206).

use regia_framework::demo::{DemoError, DemoRecording, MAX_DEMO_BYTES};
use regia_framework::extension::ExtensionManifest;

const HEAD: &str = "format_version = 1\nid = \"cheats\"\nversion = \"0.1.0\"\n";

/// A tool command or a code library that leaves the extension's folder is refused while the manifest
/// parses, before anything could run or load it. `[[content]]` has its own test in `extension_manifest.rs`.
#[test]
fn a_tool_command_or_a_library_cannot_leave_the_extension_folder() {
    for bad in [
        "../bin/x",
        "/usr/bin/python",
        "C:/Windows/x.exe",
        "bin\\\\x.exe",
        "bin/../../x",
        "NUL",
        "a.exe.",
    ] {
        let tool = format!("{HEAD}[[tool]]\nname = \"t\"\ncommand = \"{bad}\"\n");
        let error = ExtensionManifest::parse(&tool).expect_err(bad);
        assert_eq!(error.code(), "path_escape", "tool {bad}: {error:?}");
        let code = format!("{HEAD}[[code]]\nlibrary = \"{bad}\"\n");
        let error = ExtensionManifest::parse(&code).expect_err(bad);
        assert_eq!(error.code(), "path_escape", "library {bad}: {error:?}");
    }
    let fine = format!("{HEAD}[[tool]]\nname = \"t\"\ncommand = \"bin/x\"\n");
    assert!(ExtensionManifest::parse(&fine).is_ok());
}

/// A demo over the limit is refused with `TooLarge` (from its size, or after one byte past the
/// limit when the file grew after the check).
#[test]
fn a_demo_over_its_size_limit_is_refused() {
    let dir = regia_test_utils::unique_temp_dir("threat-demo-cap");
    let path = dir.join("big.regiademo");
    let file = std::fs::File::create(&path).expect("create");
    file.set_len(MAX_DEMO_BYTES + 1).expect("sparse file");
    assert!(matches!(
        DemoRecording::load(&path),
        Err(DemoError::TooLarge { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
