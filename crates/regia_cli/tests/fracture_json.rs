// SPDX-License-Identifier: MPL-2.0
//! Exercises fracture JSON output and the two generated asset files through the CLI.

use std::process::Command;

#[test]
fn json_summary_writes_fracture_and_destructible_assets() {
    let dir = regia_test_utils::unique_temp_dir("fracture-json-cli");
    let output = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args([
            "fracture",
            "--box",
            "1,0.5,0.25",
            "--chunks",
            "24",
            "--seed",
            "3",
            "--kit",
            "slab",
            "--kit-revision",
            "r3",
            "--fx-tag",
            "concrete",
            "--json",
        ])
        .arg("--out")
        .arg(dir.join("slab"))
        .output()
        .expect("run regia");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON summary");
    assert!(value.is_object(), "{value}");
    assert_eq!(value["chunks"], 24);
    assert!(dir.join("slab.fracture.ron").is_file());
    assert!(dir.join("slab.destructible.ron").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}
